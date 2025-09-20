//! Sistema de efectos visuales avanzados para COSMIC Desktop
//! 
//! Integra las mejores características de efectos visuales de Lunar
//! en el entorno de escritorio COSMIC con efectos espaciales mejorados.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::format;
use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};
use crate::drivers::framebuffer::{FramebufferDriver, Color, FramebufferInfo};
use crate::math_utils::{sin, sqrt, min, max};

/// Tipo de efecto visual
#[derive(Debug, Clone, PartialEq)]
pub enum VisualEffectType {
    /// Efecto de partículas
    Particle,
    /// Efecto de brillo/glow
    Glow,
    /// Efecto de sombra
    Shadow,
    /// Efecto de desenfoque
    Blur,
    /// Efecto de transparencia
    Transparency,
    /// Efecto de animación
    Animation,
    /// Efecto de gradiente
    Gradient,
    /// Efecto de reflexión
    Reflection,
    /// Efecto de distorsión
    Distortion,
    /// Efecto de iluminación
    Lighting,
}

/// Intensidad del efecto
#[derive(Debug, Clone, PartialEq)]
pub enum EffectIntensity {
    Low,
    Medium,
    High,
    Ultra,
}

/// Configuración de efecto visual
#[derive(Debug, Clone)]
pub struct VisualEffectConfig {
    pub effect_type: VisualEffectType,
    pub intensity: EffectIntensity,
    pub duration_ms: u32,
    pub loop_effect: bool,
    pub parameters: BTreeMap<String, String>,
}

/// Estado de efecto visual
#[derive(Debug, Clone)]
pub struct VisualEffectState {
    pub config: VisualEffectConfig,
    pub is_active: bool,
    pub current_time_ms: u32,
    pub progress: f32, // 0.0 a 1.0
    pub performance_impact: f32, // 0.0 a 1.0
}

/// Sistema de efectos visuales Lunar
pub struct CosmicVisualEffects {
    /// Efectos activos
    active_effects: BTreeMap<String, VisualEffectState>,
    /// Configuración global
    global_config: VisualEffectsGlobalConfig,
    /// Contador de frames para animaciones
    frame_counter: AtomicU32,
    /// Estado de habilitación
    effects_enabled: AtomicBool,
    /// Estadísticas de rendimiento
    performance_stats: VisualEffectsPerformanceStats,
}

/// Configuración global de efectos visuales
#[derive(Debug, Clone)]
pub struct VisualEffectsGlobalConfig {
    pub max_active_effects: usize,
    pub auto_disable_on_low_fps: bool,
    pub min_fps_threshold: f32,
    pub quality_preset: EffectQualityPreset,
    pub hardware_acceleration: bool,
}

/// Preset de calidad de efectos
#[derive(Debug, Clone)]
pub enum EffectQualityPreset {
    Performance, // Máximo rendimiento, efectos mínimos
    Balanced,    // Balance entre calidad y rendimiento
    Quality,     // Alta calidad, efectos completos
    Ultra,       // Máxima calidad, todos los efectos
}

/// Estadísticas de rendimiento de efectos
#[derive(Debug, Clone)]
pub struct VisualEffectsPerformanceStats {
    pub total_effects_rendered: u32,
    pub average_render_time_ms: f32,
    pub peak_render_time_ms: f32,
    pub effects_per_frame: f32,
    pub gpu_utilization: f32,
}

impl CosmicVisualEffects {
    /// Crear nuevo sistema de efectos visuales
    pub fn new() -> Self {
        Self {
            active_effects: BTreeMap::new(),
            global_config: VisualEffectsGlobalConfig {
                max_active_effects: 10,
                auto_disable_on_low_fps: true,
                min_fps_threshold: 30.0,
                quality_preset: EffectQualityPreset::Balanced,
                hardware_acceleration: true,
            },
            frame_counter: AtomicU32::new(0),
            effects_enabled: AtomicBool::new(true),
            performance_stats: VisualEffectsPerformanceStats {
                total_effects_rendered: 0,
                average_render_time_ms: 0.0,
                peak_render_time_ms: 0.0,
                effects_per_frame: 0.0,
                gpu_utilization: 0.0,
            },
        }
    }

    /// Activar efecto visual
    pub fn activate_effect(&mut self, effect_id: &str, config: VisualEffectConfig) -> Result<(), String> {
        if !self.effects_enabled.load(Ordering::Relaxed) {
            return Err("Efectos visuales deshabilitados".to_string());
        }

        if self.active_effects.len() >= self.global_config.max_active_effects {
            return Err("Máximo número de efectos activos alcanzado".to_string());
        }

        let state = VisualEffectState {
            config: config.clone(),
            is_active: true,
            current_time_ms: 0,
            progress: 0.0,
            performance_impact: self.calculate_performance_impact(&config),
        };

        self.active_effects.insert(effect_id.to_string(), state);
        Ok(())
    }

    /// Desactivar efecto visual
    pub fn deactivate_effect(&mut self, effect_id: &str) -> Result<(), String> {
        if let Some(state) = self.active_effects.get_mut(effect_id) {
            state.is_active = false;
            Ok(())
        } else {
            Err(format!("Efecto {} no encontrado", effect_id))
        }
    }

    /// Actualizar efectos (llamar cada frame)
    pub fn update_effects(&mut self, delta_time_ms: u32) {
        if !self.effects_enabled.load(Ordering::Relaxed) {
            return;
        }

        let frame_count = self.frame_counter.fetch_add(1, Ordering::Relaxed);
        
        // Actualizar estado de cada efecto activo
        for (effect_id, state) in self.active_effects.iter_mut() {
            if state.is_active {
                state.current_time_ms += delta_time_ms;
                
                // Calcular progreso
                if state.config.duration_ms > 0 {
                    state.progress = (state.current_time_ms as f32 / state.config.duration_ms as f32).min(1.0);
                } else {
                    state.progress = 0.0;
                }

                // Desactivar efecto si ha terminado y no es loop
                if !state.config.loop_effect && state.progress >= 1.0 {
                    state.is_active = false;
                }
            }
        }

        // Limpiar efectos inactivos
        self.active_effects.retain(|_, state| state.is_active);
    }

    /// Renderizar todos los efectos activos
    pub fn render_effects(&mut self, framebuffer: &mut FramebufferDriver) -> Result<(), String> {
        if !self.effects_enabled.load(Ordering::Relaxed) {
            return Ok(());
        }

        let start_time = 0; // En implementación real usaríamos un timer
        let mut effects_rendered = 0;

        for (effect_id, state) in &self.active_effects {
            if state.is_active {
                match state.config.effect_type {
                    VisualEffectType::Particle => {
                        self.render_particle_effect(framebuffer, state)?;
                    },
                    VisualEffectType::Glow => {
                        self.render_glow_effect(framebuffer, state)?;
                    },
                    VisualEffectType::Shadow => {
                        self.render_shadow_effect(framebuffer, state)?;
                    },
                    VisualEffectType::Blur => {
                        self.render_blur_effect(framebuffer, state)?;
                    },
                    VisualEffectType::Transparency => {
                        self.render_transparency_effect(framebuffer, state)?;
                    },
                    VisualEffectType::Animation => {
                        self.render_animation_effect(framebuffer, state)?;
                    },
                    VisualEffectType::Gradient => {
                        self.render_gradient_effect(framebuffer, state)?;
                    },
                    VisualEffectType::Reflection => {
                        self.render_reflection_effect(framebuffer, state)?;
                    },
                    VisualEffectType::Distortion => {
                        self.render_distortion_effect(framebuffer, state)?;
                    },
                    VisualEffectType::Lighting => {
                        self.render_lighting_effect(framebuffer, state)?;
                    },
                }
                effects_rendered += 1;
            }
        }

        // Actualizar estadísticas
        self.update_performance_stats(effects_rendered, start_time);

        Ok(())
    }

    /// Renderizar efecto de partículas
    fn render_particle_effect(&self, framebuffer: &mut FramebufferDriver, state: &VisualEffectState) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;
        let particle_count = match state.config.intensity {
            EffectIntensity::Low => 10,
            EffectIntensity::Medium => 25,
            EffectIntensity::High => 50,
            EffectIntensity::Ultra => 100,
        };

        for i in 0..particle_count {
            let x = ((state.current_time_ms + i * 7) % width as u32) as u32;
            let y = ((state.current_time_ms * 2 + i * 11) % height as u32) as u32;
            
            // Color basado en el progreso del efecto
            let alpha = (255.0 * (1.0 - state.progress)) as u8;
            let color = Color::from_rgba(0, 255, 255, alpha);
            
            framebuffer.put_pixel(x, y, color);
        }

        Ok(())
    }

    /// Renderizar efecto de brillo
    fn render_glow_effect(&self, framebuffer: &mut FramebufferDriver, state: &VisualEffectState) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;
        let glow_radius = match state.config.intensity {
            EffectIntensity::Low => 2,
            EffectIntensity::Medium => 4,
            EffectIntensity::High => 8,
            EffectIntensity::Ultra => 16,
        };

        // Simular efecto de glow aplicando brillo a píxeles existentes
        for y in 0..height {
            for x in 0..width {
                if (x + y) % glow_radius == 0 {
                    // Aplicar efecto de brillo suave (simulado)
                    let enhanced_color = Color::from_rgba(255, 255, 255, 128);
                    framebuffer.put_pixel(x, y, enhanced_color);
                }
            }
        }

        Ok(())
    }

    /// Renderizar efecto de sombra
    fn render_shadow_effect(&self, _framebuffer: &mut FramebufferDriver, _state: &VisualEffectState) -> Result<(), String> {
        // Simular efecto de sombra (en implementación real sería más complejo)
        Ok(())
    }

    /// Renderizar efecto de desenfoque
    fn render_blur_effect(&self, _framebuffer: &mut FramebufferDriver, _state: &VisualEffectState) -> Result<(), String> {
        // Simular efecto de blur (en implementación real sería más complejo)
        Ok(())
    }

    /// Renderizar efecto de transparencia
    fn render_transparency_effect(&self, _framebuffer: &mut FramebufferDriver, _state: &VisualEffectState) -> Result<(), String> {
        // Simular efecto de transparencia
        Ok(())
    }

    /// Renderizar efecto de animación
    fn render_animation_effect(&self, framebuffer: &mut FramebufferDriver, state: &VisualEffectState) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;
        
        // Crear patrón animado basado en el tiempo
        let animation_speed = match state.config.intensity {
            EffectIntensity::Low => 1,
            EffectIntensity::Medium => 2,
            EffectIntensity::High => 4,
            EffectIntensity::Ultra => 8,
        };

        for y in 0..height {
            for x in 0..width {
                let wave = sin((x as f32 * 0.1) + (state.current_time_ms as f32 * animation_speed as f32 * 0.01));
                let brightness = ((wave + 1.0) * 127.5) as u8;
                let color = Color::from_rgba(brightness, brightness, brightness, 128);
                
                if (x + y) % 4 == 0 {
                    framebuffer.put_pixel(x, y, color);
                }
            }
        }

        Ok(())
    }

    /// Renderizar efecto de gradiente
    fn render_gradient_effect(&self, framebuffer: &mut FramebufferDriver, state: &VisualEffectState) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;
        
        for y in 0..height {
            let gradient_progress = y as f32 / height as f32;
            let red = (gradient_progress * 255.0) as u8;
            let blue = ((1.0 - gradient_progress) * 255.0) as u8;
            let green = 128;
            let alpha = (255.0 * state.progress) as u8;
            
            let color = Color::from_rgba(red, green, blue, alpha);
            
            for x in 0..width {
                if x % 8 == 0 { // Renderizar cada 8 píxeles para optimizar
                    framebuffer.put_pixel(x, y, color);
                }
            }
        }

        Ok(())
    }

    /// Renderizar efecto de reflexión
    fn render_reflection_effect(&self, _framebuffer: &mut FramebufferDriver, _state: &VisualEffectState) -> Result<(), String> {
        // Simular efecto de reflexión
        Ok(())
    }

    /// Renderizar efecto de distorsión
    fn render_distortion_effect(&self, _framebuffer: &mut FramebufferDriver, _state: &VisualEffectState) -> Result<(), String> {
        // Simular efecto de distorsión
        Ok(())
    }

    /// Renderizar efecto de iluminación
    fn render_lighting_effect(&self, framebuffer: &mut FramebufferDriver, state: &VisualEffectState) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;
        
        // Crear efecto de iluminación radial
        let center_x = width / 2;
        let center_y = height / 2;
        let max_distance = sqrt((center_x as f32 * center_x as f32 + center_y as f32 * center_y as f32) as f64);

        for y in 0..height {
            for x in 0..width {
                let dx = x as i32 - center_x as i32;
                let dy = y as i32 - center_y as i32;
                let distance = sqrt((dx * dx + dy * dy) as f64);
                
                let light_intensity = 1.0 - min((distance / max_distance) as f32, 1.0);
                let brightness = (light_intensity * 255.0 * state.progress) as u8;
                
                if brightness > 0 {
                    let color = Color::from_rgba(brightness, brightness, brightness, brightness);
                    framebuffer.put_pixel(x, y, color);
                }
            }
        }

        Ok(())
    }

    /// Mejorar brillo de color
    fn enhance_color_brightness(&self, color: Color, factor: f32) -> Color {
        let (r, g, b, a) = color.to_rgba();
            let new_r = min(r as f32 * factor, 255.0) as u8;
            let new_g = min(g as f32 * factor, 255.0) as u8;
            let new_b = min(b as f32 * factor, 255.0) as u8;
        
        Color::from_rgba(new_r, new_g, new_b, a)
    }

    /// Calcular impacto en el rendimiento
    fn calculate_performance_impact(&self, config: &VisualEffectConfig) -> f32 {
        let base_impact = match config.effect_type {
            VisualEffectType::Particle => 0.3,
            VisualEffectType::Glow => 0.2,
            VisualEffectType::Shadow => 0.4,
            VisualEffectType::Blur => 0.8,
            VisualEffectType::Transparency => 0.1,
            VisualEffectType::Animation => 0.5,
            VisualEffectType::Gradient => 0.2,
            VisualEffectType::Reflection => 0.6,
            VisualEffectType::Distortion => 0.7,
            VisualEffectType::Lighting => 0.9,
        };

        let intensity_multiplier = match config.intensity {
            EffectIntensity::Low => 0.5,
            EffectIntensity::Medium => 1.0,
            EffectIntensity::High => 1.5,
            EffectIntensity::Ultra => 2.0,
        };

        base_impact * intensity_multiplier
    }

    /// Actualizar estadísticas de rendimiento
    fn update_performance_stats(&mut self, effects_rendered: usize, _start_time: u32) {
        self.performance_stats.total_effects_rendered += effects_rendered as u32;
        self.performance_stats.effects_per_frame = effects_rendered as f32;
        
        // En implementación real calcularíamos el tiempo real de renderizado
        self.performance_stats.average_render_time_ms = effects_rendered as f32 * 0.1;
        self.performance_stats.peak_render_time_ms = effects_rendered as f32 * 0.2;
    }

    /// Habilitar/deshabilitar efectos
    pub fn set_effects_enabled(&self, enabled: bool) {
        self.effects_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Verificar si los efectos están habilitados
    pub fn is_effects_enabled(&self) -> bool {
        self.effects_enabled.load(Ordering::Relaxed)
    }

    /// Obtener estadísticas de rendimiento
    pub fn get_performance_stats(&self) -> &VisualEffectsPerformanceStats {
        &self.performance_stats
    }

    /// Obtener número de efectos activos
    pub fn get_active_effects_count(&self) -> usize {
        self.active_effects.len()
    }

    /// Limpiar todos los efectos
    pub fn clear_all_effects(&mut self) {
        self.active_effects.clear();
    }

    /// Configurar preset de calidad
    pub fn set_quality_preset(&mut self, preset: EffectQualityPreset) {
        // Ajustar configuración según el preset
        match preset {
            EffectQualityPreset::Performance => {
                self.global_config.max_active_effects = 3;
                self.global_config.min_fps_threshold = 60.0;
            },
            EffectQualityPreset::Balanced => {
                self.global_config.max_active_effects = 6;
                self.global_config.min_fps_threshold = 45.0;
            },
            EffectQualityPreset::Quality => {
                self.global_config.max_active_effects = 10;
                self.global_config.min_fps_threshold = 30.0;
            },
            EffectQualityPreset::Ultra => {
                self.global_config.max_active_effects = 15;
                self.global_config.min_fps_threshold = 20.0;
            },
        }
        
        // Asignar el preset después del match
        self.global_config.quality_preset = preset;
    }

    /// Obtener configuración global
    pub fn get_global_config(&self) -> &VisualEffectsGlobalConfig {
        &self.global_config
    }

    /// Obtener efectos activos
    pub fn get_active_effects(&self) -> &BTreeMap<String, VisualEffectState> {
        &self.active_effects
    }
}
