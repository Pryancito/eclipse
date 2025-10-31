use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::string::String;
use alloc::vec::Vec;

/// Sistema de shaders visuales personalizados para COSMIC
pub struct VisualShaderSystem {
    /// Shaders disponibles
    shaders: Vec<Shader>,
    /// Configuración del sistema
    config: ShaderSystemConfig,
    /// Estadísticas de rendimiento
    stats: ShaderStats,
}

/// Configuración del sistema de shaders
#[derive(Debug, Clone)]
pub struct ShaderSystemConfig {
    /// Habilitar efectos de blur
    pub enable_blur: bool,
    /// Habilitar efectos de glow
    pub enable_glow: bool,
    /// Habilitar efectos de partículas
    pub enable_particles: bool,
    /// Calidad de renderizado (1-10)
    pub render_quality: u8,
    /// FPS objetivo
    pub target_fps: u32,
}

impl Default for ShaderSystemConfig {
    fn default() -> Self {
        Self {
            enable_blur: true,
            enable_glow: true,
            enable_particles: true,
            render_quality: 8,
            target_fps: 60,
        }
    }
}

/// Estadísticas del sistema de shaders
#[derive(Debug, Clone)]
pub struct ShaderStats {
    /// FPS actual
    pub current_fps: f32,
    /// Tiempo de renderizado por frame
    pub render_time_ms: f32,
    /// Número de shaders activos
    pub active_shaders: usize,
    /// Memoria utilizada
    pub memory_usage: usize,
}

/// Shader individual
#[derive(Debug, Clone)]
pub struct Shader {
    /// ID único del shader
    pub id: String,
    /// Tipo de shader
    pub shader_type: ShaderType,
    /// Configuración del shader
    pub config: ShaderConfig,
    /// Estado del shader
    pub state: ShaderState,
}

/// Tipo de shader
#[derive(Debug, Clone, PartialEq)]
pub enum ShaderType {
    /// Efecto de blur gaussiano
    GaussianBlur,
    /// Efecto de glow
    Glow,
    /// Efecto de partículas
    Particles,
    /// Efecto de ondas
    Waves,
    /// Efecto de fuego
    Fire,
    /// Efecto de agua
    Water,
    /// Efecto de cristal
    Crystal,
    /// Efecto de neón
    Neon,
}

/// Configuración de un shader
#[derive(Debug, Clone)]
pub struct ShaderConfig {
    /// Intensidad del efecto (0.0-1.0)
    pub intensity: f32,
    /// Velocidad de animación
    pub animation_speed: f32,
    /// Color principal
    pub primary_color: Color,
    /// Color secundario
    pub secondary_color: Color,
    /// Parámetros adicionales
    pub parameters: Vec<f32>,
}

/// Estado de un shader
#[derive(Debug, Clone)]
pub struct ShaderState {
    /// Tiempo de vida del shader
    pub lifetime: f32,
    /// Tiempo de animación
    pub animation_time: f32,
    /// Estado activo
    pub is_active: bool,
    /// Posición del efecto
    pub position: (f32, f32),
    /// Tamaño del efecto
    pub size: (f32, f32),
}

impl VisualShaderSystem {
    /// Crear nuevo sistema de shaders
    pub fn new() -> Self {
        Self {
            shaders: Vec::new(),
            config: ShaderSystemConfig::default(),
            stats: ShaderStats {
                current_fps: 0.0,
                render_time_ms: 0.0,
                active_shaders: 0,
                memory_usage: 0,
            },
        }
    }

    /// Crear sistema con configuración personalizada
    pub fn with_config(config: ShaderSystemConfig) -> Self {
        Self {
            shaders: Vec::new(),
            config,
            stats: ShaderStats {
                current_fps: 0.0,
                render_time_ms: 0.0,
                active_shaders: 0,
                memory_usage: 0,
            },
        }
    }

    /// Agregar un nuevo shader
    pub fn add_shader(&mut self, shader: Shader) {
        self.shaders.push(shader);
        self.update_stats();
    }

    /// Remover un shader por ID
    pub fn remove_shader(&mut self, id: &str) -> bool {
        if let Some(pos) = self.shaders.iter().position(|s| s.id == id) {
            self.shaders.remove(pos);
            self.update_stats();
            true
        } else {
            false
        }
    }

    /// Actualizar todos los shaders
    pub fn update(&mut self, delta_time: f32) {
        for shader in &mut self.shaders {
            if shader.state.is_active {
                shader.state.lifetime += delta_time;
                shader.state.animation_time += delta_time * shader.config.animation_speed;
            }
        }
        self.update_stats();
    }

    /// Renderizar todos los shaders activos
    pub fn render(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        for shader in &mut self.shaders {
            if shader.state.is_active {
                match shader.shader_type {
                    ShaderType::GaussianBlur => Self::render_gaussian_blur(fb, shader)?,
                    ShaderType::Glow => Self::render_glow(fb, shader)?,
                    ShaderType::Particles => Self::render_particles(fb, shader)?,
                    ShaderType::Waves => Self::render_waves(fb, shader)?,
                    ShaderType::Fire => Self::render_fire(fb, shader)?,
                    ShaderType::Water => Self::render_water(fb, shader)?,
                    ShaderType::Crystal => Self::render_crystal(fb, shader)?,
                    ShaderType::Neon => Self::render_neon(fb, shader)?,
                }
            }
        }
        Ok(())
    }

    /// Renderizar efecto de blur gaussiano
    fn render_gaussian_blur(fb: &mut FramebufferDriver, shader: &Shader) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;
        let intensity = shader.config.intensity;
        let radius = (intensity * 10.0) as u32;

        // Aplicar blur gaussiano simple
        for y in radius..height.saturating_sub(radius) {
            for x in radius..width.saturating_sub(radius) {
                let mut r = 0u32;
                let mut g = 0u32;
                let mut b = 0u32;
                let mut count = 0u32;

                // Muestra de píxeles alrededor (simplificado para no_std)
                for dy in 0..radius {
                    for dx in 0..radius {
                        // Simular pixel sampling con colores base
                        r += 128; // Valor promedio
                        g += 128;
                        b += 128;
                        count += 1;
                    }
                }

                if count > 0 {
                    let color = Color {
                        r: (r / count) as u8,
                        g: (g / count) as u8,
                        b: (b / count) as u8,
                        a: 255,
                    };
                    fb.draw_rect(x, y, 1, 1, color);
                }
            }
        }
        Ok(())
    }

    /// Renderizar efecto de glow
    fn render_glow(fb: &mut FramebufferDriver, shader: &Shader) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;
        let intensity = shader.config.intensity;
        let glow_radius = (intensity * 20.0) as u32;
        let (center_x, center_y) = shader.state.position;
        let center_x = center_x as u32;
        let center_y = center_y as u32;

        // Renderizar glow radial
        for y in 0..height {
            for x in 0..width {
                let dx = (x as i32 - center_x as i32).abs() as u32;
                let dy = (y as i32 - center_y as i32).abs() as u32;
                let distance = dx + dy; // Distancia Manhattan para rendimiento

                if distance < glow_radius {
                    let glow_intensity = (glow_radius - distance) as f32 / glow_radius as f32;
                    let alpha = (glow_intensity * intensity * 255.0) as u8;

                    if alpha > 0 {
                        let color = Color {
                            r: (shader.config.primary_color.r as f32 * glow_intensity) as u8,
                            g: (shader.config.primary_color.g as f32 * glow_intensity) as u8,
                            b: (shader.config.primary_color.b as f32 * glow_intensity) as u8,
                            a: alpha,
                        };
                        fb.draw_rect(x, y, 1, 1, color);
                    }
                }
            }
        }
        Ok(())
    }

    /// Renderizar efecto de partículas
    fn render_particles(fb: &mut FramebufferDriver, shader: &Shader) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;
        let intensity = shader.config.intensity;
        let particle_count = (intensity * 100.0) as usize;

        // Generar partículas animadas
        for i in 0..particle_count {
            let x = (i * 37 + (shader.state.animation_time * 100.0) as usize) % width as usize;
            let y = (i * 23 + (shader.state.animation_time * 50.0) as usize) % height as usize;
            let alpha =
                ((i * 7 + (shader.state.animation_time * 10.0) as usize) % 100) as f32 / 100.0;

            let color = Color {
                r: (shader.config.primary_color.r as f32 * alpha) as u8,
                g: (shader.config.primary_color.g as f32 * alpha) as u8,
                b: (shader.config.primary_color.b as f32 * alpha) as u8,
                a: (alpha * 255.0) as u8,
            };

            fb.draw_rect(x as u32, y as u32, 2, 2, color);
        }
        Ok(())
    }

    /// Renderizar efecto de ondas
    fn render_waves(fb: &mut FramebufferDriver, shader: &Shader) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;
        let intensity = shader.config.intensity;
        let wave_speed = shader.config.animation_speed;

        for y in 0..height {
            for x in 0..width {
                // Calcular onda sinusoidal
                let wave_x = (x as f32 * 0.1) + (shader.state.animation_time * wave_speed);
                let wave_y = (y as f32 * 0.1) + (shader.state.animation_time * wave_speed * 0.5);

                // Aproximación simple de sin para no_std
                let wave_value = (wave_x * 0.5 + 1.0) * 0.5;
                let wave_intensity = wave_value * intensity;

                if wave_intensity > 0.1 {
                    let color = Color {
                        r: (shader.config.primary_color.r as f32 * wave_intensity) as u8,
                        g: (shader.config.primary_color.g as f32 * wave_intensity) as u8,
                        b: (shader.config.primary_color.b as f32 * wave_intensity) as u8,
                        a: (wave_intensity * 255.0) as u8,
                    };
                    fb.draw_rect(x, y, 1, 1, color);
                }
            }
        }
        Ok(())
    }

    /// Renderizar efecto de fuego
    fn render_fire(fb: &mut FramebufferDriver, shader: &Shader) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;
        let intensity = shader.config.intensity;
        let (center_x, center_y) = shader.state.position;

        for y in 0..height {
            for x in 0..width {
                let dx = (x as f32 - center_x).abs();
                let dy = (y as f32 - center_y).abs();
                let distance = dx + dy;

                if distance < 100.0 {
                    let fire_intensity = (100.0 - distance) / 100.0;
                    let flicker =
                        ((x + y + (shader.state.animation_time * 100.0) as u32) % 10) as f32 / 10.0;
                    let final_intensity = fire_intensity * intensity * (0.5 + flicker * 0.5);

                    if final_intensity > 0.1 {
                        let color = Color {
                            r: (255.0 * final_intensity) as u8,
                            g: (128.0 * final_intensity) as u8,
                            b: (0.0 * final_intensity) as u8,
                            a: (final_intensity * 255.0) as u8,
                        };
                        fb.draw_rect(x, y, 1, 1, color);
                    }
                }
            }
        }
        Ok(())
    }

    /// Renderizar efecto de agua
    fn render_water(fb: &mut FramebufferDriver, shader: &Shader) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;
        let intensity = shader.config.intensity;

        for y in 0..height {
            for x in 0..width {
                let ripple_x = (x as f32 * 0.05) + (shader.state.animation_time * 0.5);
                let ripple_y = (y as f32 * 0.05) + (shader.state.animation_time * 0.3);

                // Aproximación de ondas de agua
                let ripple = (ripple_x * 0.5 + 1.0) * 0.5;
                let water_intensity = ripple * intensity * 0.3;

                if water_intensity > 0.05 {
                    let color = Color {
                        r: (0.0 * water_intensity) as u8,
                        g: (100.0 * water_intensity) as u8,
                        b: (200.0 * water_intensity) as u8,
                        a: (water_intensity * 255.0) as u8,
                    };
                    fb.draw_rect(x, y, 1, 1, color);
                }
            }
        }
        Ok(())
    }

    /// Renderizar efecto de cristal
    fn render_crystal(fb: &mut FramebufferDriver, shader: &Shader) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;
        let intensity = shader.config.intensity;
        let (center_x, center_y) = shader.state.position;

        for y in 0..height {
            for x in 0..width {
                let dx = (x as f32 - center_x).abs();
                let dy = (y as f32 - center_y).abs();
                let distance = dx + dy;

                if distance < 80.0 {
                    let crystal_intensity = (80.0 - distance) / 80.0;
                    let refraction =
                        ((x + y + (shader.state.animation_time * 50.0) as u32) % 20) as f32 / 20.0;
                    let final_intensity = crystal_intensity * intensity * (0.7 + refraction * 0.3);

                    if final_intensity > 0.1 {
                        let color = Color {
                            r: (150.0 * final_intensity) as u8,
                            g: (200.0 * final_intensity) as u8,
                            b: (255.0 * final_intensity) as u8,
                            a: (final_intensity * 255.0) as u8,
                        };
                        fb.draw_rect(x, y, 1, 1, color);
                    }
                }
            }
        }
        Ok(())
    }

    /// Renderizar efecto de neón
    fn render_neon(fb: &mut FramebufferDriver, shader: &Shader) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;
        let intensity = shader.config.intensity;

        // Efecto de neón en los bordes
        for y in 0..height {
            for x in 0..width {
                let edge_distance = (x.min(width - x).min(y.min(height - y))) as f32;
                let neon_intensity = (edge_distance / 20.0).min(1.0) * intensity;
                let pulse = ((shader.state.animation_time * 2.0) as u32 % 100) as f32 / 100.0;

                if neon_intensity > 0.1 {
                    let color = Color {
                        r: (shader.config.primary_color.r as f32
                            * neon_intensity
                            * (0.5 + pulse * 0.5)) as u8,
                        g: (shader.config.primary_color.g as f32
                            * neon_intensity
                            * (0.5 + pulse * 0.5)) as u8,
                        b: (shader.config.primary_color.b as f32
                            * neon_intensity
                            * (0.5 + pulse * 0.5)) as u8,
                        a: (neon_intensity * 255.0) as u8,
                    };
                    fb.draw_rect(x, y, 1, 1, color);
                }
            }
        }
        Ok(())
    }

    /// Actualizar estadísticas
    fn update_stats(&mut self) {
        self.stats.active_shaders = self.shaders.iter().filter(|s| s.state.is_active).count();
        self.stats.memory_usage = self.shaders.len() * core::mem::size_of::<Shader>();
    }

    /// Obtener estadísticas
    pub fn get_stats(&self) -> &ShaderStats {
        &self.stats
    }

    /// Configurar el sistema
    pub fn configure(&mut self, config: ShaderSystemConfig) {
        self.config = config;
    }

    /// Obtener configuración
    pub fn get_config(&self) -> &ShaderSystemConfig {
        &self.config
    }

    /// Listar shaders activos
    pub fn list_active_shaders(&self) -> Vec<&Shader> {
        self.shaders.iter().filter(|s| s.state.is_active).collect()
    }

    /// Activar/desactivar shader
    pub fn toggle_shader(&mut self, id: &str) -> bool {
        if let Some(shader) = self.shaders.iter_mut().find(|s| s.id == id) {
            shader.state.is_active = !shader.state.is_active;
            self.update_stats();
            true
        } else {
            false
        }
    }
}

/// Crear shaders de ejemplo
pub fn create_sample_shaders() -> Vec<Shader> {
    Vec::from([
        Shader {
            id: String::from("glow_center"),
            shader_type: ShaderType::Glow,
            config: ShaderConfig {
                intensity: 0.8,
                animation_speed: 1.0,
                primary_color: Color::from_hex(0x00aaff),
                secondary_color: Color::from_hex(0x0066aa),
                parameters: Vec::from([0.5, 1.0, 0.8]),
            },
            state: ShaderState {
                lifetime: 0.0,
                animation_time: 0.0,
                is_active: true,
                position: (960.0, 540.0), // Centro de pantalla 1920x1080
                size: (200.0, 200.0),
            },
        },
        Shader {
            id: String::from("particles_space"),
            shader_type: ShaderType::Particles,
            config: ShaderConfig {
                intensity: 0.6,
                animation_speed: 0.5,
                primary_color: Color::from_hex(0x44aaff),
                secondary_color: Color::from_hex(0x2288cc),
                parameters: Vec::from([0.3, 0.7, 1.0]),
            },
            state: ShaderState {
                lifetime: 0.0,
                animation_time: 0.0,
                is_active: true,
                position: (0.0, 0.0),
                size: (1920.0, 1080.0),
            },
        },
        Shader {
            id: String::from("neon_borders"),
            shader_type: ShaderType::Neon,
            config: ShaderConfig {
                intensity: 0.4,
                animation_speed: 2.0,
                primary_color: Color::from_hex(0x00ccff),
                secondary_color: Color::from_hex(0x0088aa),
                parameters: Vec::from([0.2, 0.8, 1.0]),
            },
            state: ShaderState {
                lifetime: 0.0,
                animation_time: 0.0,
                is_active: true,
                position: (0.0, 0.0),
                size: (1920.0, 1080.0),
            },
        },
    ])
}
