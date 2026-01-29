//! Driver DRM (Direct Rendering Manager) para Eclipse OS
//!
//! Este módulo implementa la interfaz del kernel para el sistema DRM,
//! permitiendo que el kernel controle la pantalla y se comunique
//! con el sistema DRM de userland.

use crate::desktop_ai::{Point, Rect};
use crate::drivers::framebuffer::{Color, FramebufferDriver, FramebufferInfo, PixelFormat};
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ptr;

/// Tipos de shader soportados
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShaderType {
    ColorAdjust,
    Blur,
    Sharpen,
    EdgeDetection,
    Grayscale,
    Sepia,
    Invert,
    Brightness,
    Contrast,
    Saturation,
}

/// Parámetros de shader
#[derive(Debug, Clone)]
pub struct ShaderParams {
    pub intensity: f32,
    pub color: Color,
    pub matrix: [f32; 9], // Matriz 3x3 para transformaciones
}

impl Default for ShaderParams {
    fn default() -> Self {
        Self {
            intensity: 1.0,
            color: Color::WHITE,
            matrix: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        }
    }
}

/// Modos de blending para compositing
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BlendMode {
    Normal,
    Multiply,
    Screen,
    Overlay,
    SoftLight,
    HardLight,
    ColorDodge,
    ColorBurn,
    Darken,
    Lighten,
    Difference,
    Exclusion,
}

/// Matriz de transformación 3x3
#[derive(Debug, Clone, Copy)]
pub struct TransformMatrix {
    pub m: [f32; 9],
}

impl Default for TransformMatrix {
    fn default() -> Self {
        Self {
            m: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        }
    }
}

impl TransformMatrix {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn translate(x: f32, y: f32) -> Self {
        Self {
            m: [1.0, 0.0, x, 0.0, 1.0, y, 0.0, 0.0, 1.0],
        }
    }

    pub fn scale(sx: f32, sy: f32) -> Self {
        Self {
            m: [sx, 0.0, 0.0, 0.0, sy, 0.0, 0.0, 0.0, 1.0],
        }
    }

    pub fn rotate(angle: f32) -> Self {
        // Implementación simple de cos y sin para no_std
        let cos_a = Self::simple_cos(angle);
        let sin_a = Self::simple_sin(angle);
        Self {
            m: [cos_a, -sin_a, 0.0, sin_a, cos_a, 0.0, 0.0, 0.0, 1.0],
        }
    }

    /// Implementación simple de cos para no_std
    fn simple_cos(angle: f32) -> f32 {
        // Aproximación simple usando serie de Taylor
        let x = angle % (2.0 * 3.14159265359);
        let x2 = x * x;
        let x4 = x2 * x2;
        let x6 = x4 * x2;
        1.0 - x2 / 2.0 + x4 / 24.0 - x6 / 720.0
    }

    /// Implementación simple de sin para no_std
    fn simple_sin(angle: f32) -> f32 {
        // Aproximación simple usando serie de Taylor
        let x = angle % (2.0 * 3.14159265359);
        let x2 = x * x;
        let x3 = x2 * x;
        let x5 = x3 * x2;
        let x7 = x5 * x2;
        x - x3 / 6.0 + x5 / 120.0 - x7 / 5040.0
    }
}

/// Textura GPU
#[derive(Debug, Clone)]
pub struct GpuTexture {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    pub gpu_handle: Option<u32>, // Handle del GPU
}

/// Capa de compositing
#[derive(Debug, Clone)]
pub struct CompositingLayer {
    pub id: u32,
    pub texture_id: Option<u32>,
    pub rect: Rect,
    pub blend_mode: BlendMode,
    pub alpha: f32,
    pub transform: TransformMatrix,
    pub visible: bool,
    pub z_order: i32,
}

/// Estados del driver DRM
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DrmDriverState {
    Uninitialized,
    Initializing,
    Ready,
    Error,
    Suspended,
}

/// Información del dispositivo DRM
#[derive(Debug, Clone)]
pub struct DrmDeviceInfo {
    pub device_path: String,
    pub device_fd: i32,
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    pub supports_hardware_acceleration: bool,
    pub supports_double_buffering: bool,
    pub supports_vsync: bool,
}

/// Operaciones DRM soportadas
#[derive(Debug, Clone)]
pub enum DrmOperation {
    SetMode {
        width: u32,
        height: u32,
        refresh_rate: u32,
    },
    ClearScreen {
        color: Color,
    },
    DrawPixel {
        point: Point,
        color: Color,
    },
    DrawRect {
        rect: Rect,
        color: Color,
    },
    Blit {
        src_rect: Rect,
        dst_rect: Rect,
    },
    FlipBuffer,
    EnableVsync,
    DisableVsync,
    // Nuevas operaciones aceleradas
    ScrollUp {
        pixels: u32,
    },
    ScrollDown {
        pixels: u32,
    },
    ScrollLeft {
        pixels: u32,
    },
    ScrollRight {
        pixels: u32,
    },
    LoadTexture {
        id: u32,
        data: Vec<u8>,
        width: u32,
        height: u32,
    },
    DrawTexture {
        texture_id: u32,
        src_rect: Rect,
        dst_rect: Rect,
    },
    ApplyShader {
        shader_type: ShaderType,
        params: ShaderParams,
    },
    CompositeLayer {
        layer_id: u32,
        blend_mode: BlendMode,
        alpha: f32,
    },
    Transform {
        matrix: TransformMatrix,
    },
    EnableHardwareAcceleration,
    DisableHardwareAcceleration,
}

/// Driver DRM del kernel
#[derive(Debug, Clone)]
pub struct DrmDriver {
    pub info: DrmDeviceInfo,
    pub state: DrmDriverState,
    pub framebuffer: Option<FramebufferDriver>,
    pub current_mode: (u32, u32),
    pub is_double_buffering: bool,
    pub is_vsync_enabled: bool,
    // Nuevas capacidades avanzadas
    pub textures: BTreeMap<u32, GpuTexture>,
    pub layers: BTreeMap<u32, CompositingLayer>,
    pub hardware_acceleration_enabled: bool,
    pub next_texture_id: u32,
    pub next_layer_id: u32,
    pub performance_stats: DrmPerformanceStats,
    // Nuevos campos para mejoras
    pub max_textures: u32,
    pub max_gpu_memory: u64,
    pub current_gpu_memory: u64,
    pub error_count: u32,
    pub last_error: Option<String>,
}

/// Límites de recursos DRM
pub const MAX_TEXTURES: u32 = 256;
pub const MAX_LAYERS: u32 = 64;
pub const MAX_GPU_MEMORY: u64 = 512 * 1024 * 1024; // 512 MB
pub const MAX_TEXTURE_SIZE: u32 = 8192; // 8K max dimension

impl DrmDriver {
    /// Crear una nueva instancia del driver DRM
    pub fn new() -> Self {
        Self {
            info: DrmDeviceInfo {
                device_path: "/dev/dri/card0".to_string(),
                device_fd: -1,
                width: 1920,
                height: 1080,
                bpp: 32,
                supports_hardware_acceleration: true,
                supports_double_buffering: true,
                supports_vsync: true,
            },
            state: DrmDriverState::Uninitialized,
            framebuffer: None,
            current_mode: (1920, 1080),
            is_double_buffering: false,
            is_vsync_enabled: false,
            // Inicializar nuevas capacidades
            textures: BTreeMap::new(),
            layers: BTreeMap::new(),
            hardware_acceleration_enabled: false,
            next_texture_id: 1,
            next_layer_id: 1,
            performance_stats: DrmPerformanceStats {
                frames_rendered: 0,
                scroll_operations: 0,
                texture_operations: 0,
                composite_operations: 0,
                average_frame_time: 0.0,
                average_scroll_time: 0.0,
                gpu_memory_used: 0,
                cpu_usage_percent: 0.0,
            },
            // Inicializar límites de recursos
            max_textures: MAX_TEXTURES,
            max_gpu_memory: MAX_GPU_MEMORY,
            current_gpu_memory: 0,
            error_count: 0,
            last_error: None,
        }
    }

    /// Inicializar el driver DRM
    pub fn initialize(
        &mut self,
        framebuffer_info: Option<FramebufferInfo>,
    ) -> Result<(), &'static str> {
        self.state = DrmDriverState::Initializing;

        // Simular apertura del dispositivo DRM
        // En una implementación real, esto usaría syscalls del kernel
        self.info.device_fd = 0; // Simular file descriptor

        // Configurar modo por defecto
        self.current_mode = (self.info.width, self.info.height);

        // Crear framebuffer si se proporciona información
        if let Some(_fb_info) = framebuffer_info {
            let framebuffer = FramebufferDriver::new();
            self.framebuffer = Some(framebuffer);
        }

        // Simular configuración de hardware
        self.configure_hardware()?;

        self.state = DrmDriverState::Ready;
        Ok(())
    }

    /// Configurar hardware DRM
    fn configure_hardware(&mut self) -> Result<(), &'static str> {
        // Simular configuración de registros DRM
        // En una implementación real, esto configuraría los registros de la GPU

        // Configurar modo de pantalla
        self.set_mode(self.current_mode.0, self.current_mode.1, 60)?;

        // Habilitar aceleración hardware si está disponible
        if self.info.supports_hardware_acceleration {
            self.enable_hardware_acceleration()?;
        }

        // Configurar doble buffer si está disponible
        if self.info.supports_double_buffering {
            self.enable_double_buffering()?;
        }

        Ok(())
    }

    /// Establecer modo de pantalla
    pub fn set_mode(
        &mut self,
        width: u32,
        height: u32,
        refresh_rate: u32,
    ) -> Result<(), &'static str> {
        if !self.is_ready() {
            return Err("Driver DRM no está listo");
        }

        // Simular cambio de modo
        self.current_mode = (width, height);
        self.info.width = width;
        self.info.height = height;

        // En una implementación real, esto configuraría los registros de la GPU
        // para cambiar la resolución y frecuencia de refresco

        Ok(())
    }

    /// Habilitar aceleración hardware
    fn enable_hardware_acceleration(&mut self) -> Result<(), &'static str> {
        // Simular habilitación de aceleración hardware
        // En una implementación real, esto configuraría los registros de la GPU
        Ok(())
    }

    /// Habilitar doble buffer
    fn enable_double_buffering(&mut self) -> Result<(), &'static str> {
        self.is_double_buffering = true;
        // En una implementación real, esto configuraría el doble buffer
        Ok(())
    }

    /// Ejecutar operación DRM
    pub fn execute_operation(&mut self, operation: DrmOperation) -> Result<(), &'static str> {
        // Validar estado del driver
        self.validate_ready()?;

        match operation {
            DrmOperation::SetMode {
                width,
                height,
                refresh_rate,
            } => self.set_mode(width, height, refresh_rate),
            DrmOperation::ClearScreen { color } => self.clear_screen(color),
            DrmOperation::DrawPixel { point, color } => self.draw_pixel(point, color),
            DrmOperation::DrawRect { rect, color } => self.draw_rect(rect, color),
            DrmOperation::Blit { src_rect, dst_rect } => self.blit(src_rect, dst_rect),
            DrmOperation::FlipBuffer => self.flip_buffer(),
            DrmOperation::EnableVsync => self.enable_vsync(),
            DrmOperation::DisableVsync => self.disable_vsync(),
            // Nuevas operaciones aceleradas
            DrmOperation::ScrollUp { pixels } => self.scroll_up_drm(pixels),
            DrmOperation::ScrollDown { pixels } => self.scroll_down_drm(pixels),
            DrmOperation::ScrollLeft { pixels } => self.scroll_left_drm(pixels),
            DrmOperation::ScrollRight { pixels } => self.scroll_right_drm(pixels),
            DrmOperation::LoadTexture {
                id,
                data,
                width,
                height,
            } => self.load_texture(id, data, width, height),
            DrmOperation::DrawTexture {
                texture_id,
                src_rect,
                dst_rect,
            } => self.draw_texture(texture_id, src_rect, dst_rect),
            DrmOperation::ApplyShader {
                shader_type,
                params,
            } => self.apply_shader(shader_type, params),
            DrmOperation::CompositeLayer {
                layer_id,
                blend_mode,
                alpha,
            } => self.composite_layer(layer_id, blend_mode, alpha),
            DrmOperation::Transform { matrix } => self.apply_transform(matrix),
            DrmOperation::EnableHardwareAcceleration => self.enable_hardware_acceleration(),
            DrmOperation::DisableHardwareAcceleration => self.disable_hardware_acceleration(),
        }
    }

    /// Limpiar pantalla
    pub fn clear_screen(&mut self, color: Color) -> Result<(), &'static str> {
        if let Some(ref mut fb) = self.framebuffer {
            fb.fill_rect(0, 0, self.current_mode.0, self.current_mode.1, color);
        }
        Ok(())
    }

    /// Dibujar pixel
    pub fn draw_pixel(&mut self, point: Point, color: Color) -> Result<(), &'static str> {
        if let Some(ref mut fb) = self.framebuffer {
            fb.put_pixel(point.x, point.y, color);
        }
        Ok(())
    }

    /// Dibujar rectángulo
    pub fn draw_rect(&mut self, rect: Rect, color: Color) -> Result<(), &'static str> {
        if let Some(ref mut fb) = self.framebuffer {
            fb.draw_rect(rect.x, rect.y, rect.width, rect.height, color);
        }
        Ok(())
    }

    /// Operación blit
    pub fn blit(&mut self, src_rect: Rect, dst_rect: Rect) -> Result<(), &'static str> {
        if let Some(ref mut fb) = self.framebuffer {
            // Simular operación blit (en una implementación real, esto usaría hardware)
            // Por ahora, solo simulamos la operación
            Ok(())
        } else {
            Ok(())
        }
    }

    /// Cambiar buffer (doble buffer)
    pub fn flip_buffer(&mut self) -> Result<(), &'static str> {
        if !self.is_double_buffering {
            return Err("Doble buffer no está habilitado");
        }

        // En una implementación real, esto cambiaría el buffer activo
        // y esperaría a que se complete el flip
        Ok(())
    }

    /// Habilitar VSync
    pub fn enable_vsync(&mut self) -> Result<(), &'static str> {
        if !self.info.supports_vsync {
            return Err("VSync no está soportado");
        }

        self.is_vsync_enabled = true;
        // En una implementación real, esto configuraría VSync en la GPU
        Ok(())
    }

    /// Deshabilitar VSync
    pub fn disable_vsync(&mut self) -> Result<(), &'static str> {
        self.is_vsync_enabled = false;
        // En una implementación real, esto deshabilitaría VSync en la GPU
        Ok(())
    }

    /// Verificar si el driver está listo
    pub fn is_ready(&self) -> bool {
        self.state == DrmDriverState::Ready
    }

    /// Obtener información del driver
    pub fn get_info(&self) -> &DrmDeviceInfo {
        &self.info
    }

    /// Obtener estado del driver
    pub fn get_state(&self) -> DrmDriverState {
        self.state
    }

    /// Obtener modo actual
    pub fn get_current_mode(&self) -> (u32, u32) {
        self.current_mode
    }

    /// Obtener referencia mutable al framebuffer
    pub fn get_framebuffer(&mut self) -> Option<&mut FramebufferDriver> {
        self.framebuffer.as_mut()
    }

    /// Sincronizar con VSync
    pub fn wait_for_vsync(&self) -> Result<(), &'static str> {
        if !self.is_vsync_enabled {
            return Err("VSync no está habilitado");
        }

        // En una implementación real, esto esperaría al próximo VSync
        // Por ahora, solo simulamos una pequeña pausa
        Ok(())
    }

    /// Obtener estadísticas del driver
    pub fn get_stats(&self) -> DrmDriverStats {
        DrmDriverStats {
            is_initialized: self.is_ready(),
            current_mode: self.current_mode,
            is_double_buffering: self.is_double_buffering,
            is_vsync_enabled: self.is_vsync_enabled,
            supports_hardware_acceleration: self.info.supports_hardware_acceleration,
            device_fd: self.info.device_fd,
        }
    }

    // ===== NUEVAS FUNCIONES DRM ACELERADAS =====

    /// Scroll hacia arriba usando DRM (ultra-rápido)
    pub fn scroll_up_drm(&mut self, pixels: u32) -> Result<(), &'static str> {
        if pixels == 0 {
            return Ok(());
        }

        let start_time = self.get_timestamp();

        // Usar operación Blit de DRM para scroll ultra-rápido
        let src_rect = Rect {
            x: 0,
            y: pixels,
            width: self.current_mode.0,
            height: self.current_mode.1 - pixels,
        };

        let dst_rect = Rect {
            x: 0,
            y: 0,
            width: self.current_mode.0,
            height: self.current_mode.1 - pixels,
        };

        // Ejecutar blit acelerado por hardware
        self.blit(src_rect, dst_rect)?;

        // Limpiar zona inferior
        let clear_rect = Rect {
            x: 0,
            y: self.current_mode.1 - pixels,
            width: self.current_mode.0,
            height: pixels,
        };

        self.draw_rect(clear_rect, Color::BLACK)?;

        // Actualizar estadísticas
        self.performance_stats.scroll_operations += 1;
        let end_time = self.get_timestamp();
        let scroll_time = (end_time - start_time) as f32 / 1000.0; // Convertir a ms
        self.performance_stats.average_scroll_time =
            (self.performance_stats.average_scroll_time + scroll_time) / 2.0;

        Ok(())
    }

    /// Scroll hacia abajo usando DRM
    pub fn scroll_down_drm(&mut self, pixels: u32) -> Result<(), &'static str> {
        if pixels == 0 {
            return Ok(());
        }

        let src_rect = Rect {
            x: 0,
            y: 0,
            width: self.current_mode.0,
            height: self.current_mode.1 - pixels,
        };

        let dst_rect = Rect {
            x: 0,
            y: pixels,
            width: self.current_mode.0,
            height: self.current_mode.1 - pixels,
        };

        self.blit(src_rect, dst_rect)?;

        // Limpiar zona superior
        let clear_rect = Rect {
            x: 0,
            y: 0,
            width: self.current_mode.0,
            height: pixels,
        };

        self.draw_rect(clear_rect, Color::BLACK)?;
        Ok(())
    }

    /// Scroll hacia la izquierda usando DRM
    pub fn scroll_left_drm(&mut self, pixels: u32) -> Result<(), &'static str> {
        if pixels == 0 {
            return Ok(());
        }

        let src_rect = Rect {
            x: pixels,
            y: 0,
            width: self.current_mode.0 - pixels,
            height: self.current_mode.1,
        };

        let dst_rect = Rect {
            x: 0,
            y: 0,
            width: self.current_mode.0 - pixels,
            height: self.current_mode.1,
        };

        self.blit(src_rect, dst_rect)?;

        // Limpiar zona derecha
        let clear_rect = Rect {
            x: self.current_mode.0 - pixels,
            y: 0,
            width: pixels,
            height: self.current_mode.1,
        };

        self.draw_rect(clear_rect, Color::BLACK)?;
        Ok(())
    }

    /// Scroll hacia la derecha usando DRM
    pub fn scroll_right_drm(&mut self, pixels: u32) -> Result<(), &'static str> {
        if pixels == 0 {
            return Ok(());
        }

        let src_rect = Rect {
            x: 0,
            y: 0,
            width: self.current_mode.0 - pixels,
            height: self.current_mode.1,
        };

        let dst_rect = Rect {
            x: pixels,
            y: 0,
            width: self.current_mode.0 - pixels,
            height: self.current_mode.1,
        };

        self.blit(src_rect, dst_rect)?;

        // Limpiar zona izquierda
        let clear_rect = Rect {
            x: 0,
            y: 0,
            width: pixels,
            height: self.current_mode.1,
        };

        self.draw_rect(clear_rect, Color::BLACK)?;
        Ok(())
    }

    /// Cargar textura en GPU
    pub fn load_texture(
        &mut self,
        id: u32,
        data: Vec<u8>,
        width: u32,
        height: u32,
    ) -> Result<(), &'static str> {
        // Validar límites
        if self.textures.len() >= self.max_textures as usize {
            self.record_error("Límite de texturas alcanzado");
            return Err("Maximum texture count reached");
        }

        if width > MAX_TEXTURE_SIZE || height > MAX_TEXTURE_SIZE {
            self.record_error("Tamaño de textura excede el máximo");
            return Err("Texture size exceeds maximum");
        }

        let texture_size = (width * height * 4) as u64;
        if self.current_gpu_memory + texture_size > self.max_gpu_memory {
            self.record_error("Memoria GPU insuficiente");
            return Err("Insufficient GPU memory");
        }

        // Verificar que los datos sean del tamaño correcto
        let expected_size = (width * height * 4) as usize;
        if data.len() != expected_size {
            self.record_error("Tamaño de datos de textura inválido");
            return Err("Invalid texture data size");
        }

        let texture = GpuTexture {
            id,
            width,
            height,
            format: PixelFormat::RGBA8888,
            data: data.clone(),
            gpu_handle: Some(self.next_texture_id),
        };

        self.textures.insert(id, texture);
        self.next_texture_id += 1;

        // Actualizar memoria GPU usada
        self.current_gpu_memory += texture_size;
        self.performance_stats.gpu_memory_used = self.current_gpu_memory;
        self.performance_stats.texture_operations += 1;

        Ok(())
    }

    /// Descargar textura de GPU (liberar memoria)
    pub fn unload_texture(&mut self, texture_id: u32) -> Result<(), &'static str> {
        if let Some(texture) = self.textures.remove(&texture_id) {
            let texture_size = (texture.width * texture.height * 4) as u64;
            self.current_gpu_memory = self.current_gpu_memory.saturating_sub(texture_size);
            self.performance_stats.gpu_memory_used = self.current_gpu_memory;
            Ok(())
        } else {
            Err("Texture not found")
        }
    }

    /// Registrar error
    fn record_error(&mut self, error: &str) {
        self.error_count += 1;
        self.last_error = Some(error.to_string());
    }

    /// Obtener último error
    pub fn get_last_error(&self) -> Option<&String> {
        self.last_error.as_ref()
    }

    /// Obtener conteo de errores
    pub fn get_error_count(&self) -> u32 {
        self.error_count
    }

    /// Limpiar errores
    pub fn clear_errors(&mut self) {
        self.error_count = 0;
        self.last_error = None;
    }

    /// Validar estado antes de operación
    fn validate_ready(&self) -> Result<(), &'static str> {
        if !self.is_ready() {
            return Err("Driver DRM no está listo");
        }
        if self.state == DrmDriverState::Error {
            return Err("Driver DRM en estado de error");
        }
        Ok(())
    }

    /// Dibujar textura usando GPU
    pub fn draw_texture(
        &mut self,
        texture_id: u32,
        src_rect: Rect,
        dst_rect: Rect,
    ) -> Result<(), &'static str> {
        if let Some(texture) = self.textures.get(&texture_id) {
            // En una implementación real, esto usaría el GPU para dibujar la textura
            // Por ahora, simulamos con operaciones de framebuffer
            if let Some(ref mut fb) = self.framebuffer {
                // Simular dibujo de textura
                for y in 0..dst_rect.height {
                    for x in 0..dst_rect.width {
                        let src_x = src_rect.x + x;
                        let src_y = src_rect.y + y;
                        let dst_x = dst_rect.x + x;
                        let dst_y = dst_rect.y + y;

                        if src_x >= 0
                            && src_y >= 0
                            && src_x < texture.width as u32
                            && src_y < texture.height as u32
                            && dst_x >= 0
                            && dst_y >= 0
                            && dst_x < self.current_mode.0 as u32
                            && dst_y < self.current_mode.1 as u32
                        {
                            let pixel_index =
                                ((src_y as u32 * texture.width + src_x as u32) * 4) as usize;
                            if pixel_index + 3 < texture.data.len() {
                                let r = texture.data[pixel_index];
                                let g = texture.data[pixel_index + 1];
                                let b = texture.data[pixel_index + 2];
                                let a = texture.data[pixel_index + 3];

                                let color = Color::rgb(r, g, b);
                                fb.put_pixel(dst_x as u32, dst_y as u32, color);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Aplicar shader a la pantalla
    pub fn apply_shader(
        &mut self,
        shader_type: ShaderType,
        params: ShaderParams,
    ) -> Result<(), &'static str> {
        if let Some(ref mut fb) = self.framebuffer {
            // Aplicar shader a cada pixel
            for y in 0..self.current_mode.1 {
                for x in 0..self.current_mode.0 {
                    let current_color = fb.get_pixel(x, y);
                    let new_color =
                        Self::apply_shader_to_pixel_static(current_color, shader_type, &params);
                    fb.put_pixel(x, y, new_color);
                }
            }
        }
        Ok(())
    }

    /// Aplicar shader a un pixel individual (función estática)
    fn apply_shader_to_pixel_static(
        color: Color,
        shader_type: ShaderType,
        params: &ShaderParams,
    ) -> Color {
        let r = color.r;
        let g = color.g;
        let b = color.b;
        let mut new_r = r as f32;
        let mut new_g = g as f32;
        let mut new_b = b as f32;
        let mut new_a = 255.0; // Alpha fijo

        match shader_type {
            ShaderType::Grayscale => {
                let gray = (new_r * 0.299 + new_g * 0.587 + new_b * 0.114) * params.intensity;
                new_r = gray;
                new_g = gray;
                new_b = gray;
            }
            ShaderType::Invert => {
                new_r = (255.0 - new_r) * params.intensity;
                new_g = (255.0 - new_g) * params.intensity;
                new_b = (255.0 - new_b) * params.intensity;
            }
            ShaderType::Brightness => {
                new_r = (new_r + params.intensity * 100.0).min(255.0).max(0.0);
                new_g = (new_g + params.intensity * 100.0).min(255.0).max(0.0);
                new_b = (new_b + params.intensity * 100.0).min(255.0).max(0.0);
            }
            ShaderType::Contrast => {
                let factor = (259.0 * (params.intensity * 255.0 + 255.0))
                    / (255.0 * (259.0 - params.intensity * 255.0));
                new_r = ((new_r - 128.0) * factor + 128.0).min(255.0).max(0.0);
                new_g = ((new_g - 128.0) * factor + 128.0).min(255.0).max(0.0);
                new_b = ((new_b - 128.0) * factor + 128.0).min(255.0).max(0.0);
            }
            _ => {
                // Shader no implementado, devolver color original
            }
        }

        Color::rgb(new_r as u8, new_g as u8, new_b as u8)
    }

    /// Componer capa con blending
    pub fn composite_layer(
        &mut self,
        layer_id: u32,
        blend_mode: BlendMode,
        alpha: f32,
    ) -> Result<(), &'static str> {
        if let Some(layer) = self.layers.get(&layer_id) {
            if !layer.visible {
                return Ok(());
            }

            // Aplicar transformación
            let transformed_rect = self.apply_transform_to_rect(layer.rect, layer.transform);

            // Aplicar blending
            self.apply_blend_mode(transformed_rect, blend_mode, alpha);

            self.performance_stats.composite_operations += 1;
        }
        Ok(())
    }

    /// Aplicar transformación
    pub fn apply_transform(&mut self, matrix: TransformMatrix) -> Result<(), &'static str> {
        // En una implementación real, esto aplicaría la transformación a todas las capas
        // Por ahora, solo actualizamos la matriz de transformación global
        Ok(())
    }

    /// Deshabilitar aceleración hardware
    pub fn disable_hardware_acceleration(&mut self) -> Result<(), &'static str> {
        self.hardware_acceleration_enabled = false;
        Ok(())
    }

    // ===== FUNCIONES AUXILIARES =====

    /// Obtener timestamp actual (simulado)
    fn get_timestamp(&self) -> u64 {
        // En una implementación real, esto usaría un timer del sistema
        0
    }

    /// Aplicar transformación a un rectángulo
    fn apply_transform_to_rect(&self, rect: Rect, transform: TransformMatrix) -> Rect {
        // Aplicar transformación 2D básica
        let new_x = (rect.x as f32 * transform.m[0]
            + rect.y as f32 * transform.m[1]
            + transform.m[2]) as u32;
        let new_y = (rect.x as f32 * transform.m[3]
            + rect.y as f32 * transform.m[4]
            + transform.m[5]) as u32;
        let new_width = (rect.width as f32 * transform.m[0]) as u32;
        let new_height = (rect.height as f32 * transform.m[4]) as u32;

        Rect {
            x: new_x,
            y: new_y,
            width: new_width,
            height: new_height,
        }
    }

    /// Aplicar modo de blending
    fn apply_blend_mode(&mut self, rect: Rect, blend_mode: BlendMode, alpha: f32) {
        // En una implementación real, esto aplicaría el blending usando GPU
        // Por ahora, solo simulamos
        match blend_mode {
            BlendMode::Normal => {
                // Blending normal (ya implementado)
            }
            BlendMode::Multiply => {
                // Multiplicar colores
            }
            BlendMode::Screen => {
                // Modo pantalla
            }
            _ => {
                // Otros modos de blending
            }
        }
    }

    /// Obtener estadísticas de rendimiento
    pub fn get_performance_stats(&self) -> &DrmPerformanceStats {
        &self.performance_stats
    }

    /// Crear nueva capa de compositing
    pub fn create_layer(&mut self, rect: Rect) -> Result<u32, &'static str> {
        if self.layers.len() >= MAX_LAYERS as usize {
            self.record_error("Límite de capas alcanzado");
            return Err("Maximum layer count reached");
        }

        let layer_id = self.next_layer_id;
        let layer = CompositingLayer {
            id: layer_id,
            texture_id: None,
            rect,
            blend_mode: BlendMode::Normal,
            alpha: 1.0,
            transform: TransformMatrix::new(),
            visible: true,
            z_order: 0,
        };

        self.layers.insert(layer_id, layer);
        self.next_layer_id += 1;
        Ok(layer_id)
    }

    /// Eliminar capa de compositing
    pub fn remove_layer(&mut self, layer_id: u32) -> Result<(), &'static str> {
        if self.layers.remove(&layer_id).is_some() {
            Ok(())
        } else {
            Err("Capa no encontrada")
        }
    }

    /// Obtener uso de memoria GPU
    pub fn get_gpu_memory_usage(&self) -> (u64, u64) {
        (self.current_gpu_memory, self.max_gpu_memory)
    }

    /// Obtener conteo de recursos
    pub fn get_resource_counts(&self) -> (usize, usize, u32, u32) {
        (
            self.textures.len(),
            self.layers.len(),
            self.max_textures,
            MAX_LAYERS,
        )
    }
}

/// Estadísticas del driver DRM
#[derive(Debug, Clone)]
pub struct DrmDriverStats {
    pub is_initialized: bool,
    pub current_mode: (u32, u32),
    pub is_double_buffering: bool,
    pub is_vsync_enabled: bool,
    pub supports_hardware_acceleration: bool,
    pub device_fd: i32,
}

/// Estadísticas de rendimiento DRM
#[derive(Debug, Clone)]
pub struct DrmPerformanceStats {
    pub frames_rendered: u64,
    pub scroll_operations: u64,
    pub texture_operations: u64,
    pub composite_operations: u64,
    pub average_frame_time: f32,
    pub average_scroll_time: f32,
    pub gpu_memory_used: u64,
    pub cpu_usage_percent: f32,
}

/// Función de conveniencia para crear un driver DRM
pub fn create_drm_driver() -> DrmDriver {
    DrmDriver::new()
}

/// Función de conveniencia para inicializar DRM con framebuffer
pub fn initialize_drm_with_framebuffer(
    framebuffer_info: Option<FramebufferInfo>,
) -> Result<DrmDriver, &'static str> {
    let mut driver = DrmDriver::new();
    driver.initialize(framebuffer_info)?;
    Ok(driver)
}
