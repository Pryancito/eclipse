//! Sistema de efectos visuales avanzados para COSMIC Desktop
//!
//! Integra las mejores características de efectos visuales de Lunar
//! en el entorno de escritorio COSMIC con efectos espaciales mejorados.

use crate::drivers::framebuffer::{Color, FramebufferDriver, FramebufferInfo};
use crate::math_utils::{max, min, sin, sqrt};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::f64::consts::PI;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// Función cos simple para compatibilidad
fn cos(x: f64) -> f64 {
    // Aproximación simple de coseno usando la serie de Taylor
    let x = x % (2.0 * PI);
    let x2 = x * x;
    1.0 - x2 / 2.0 + (x2 * x2) / 24.0 - (x2 * x2 * x2) / 720.0
}

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
    /// Efecto de partículas espaciales
    SpaceParticles,
    /// Efecto de nebulosa
    Nebula,
    /// Efecto de estrellas en movimiento
    MovingStars,
    /// Efecto de galaxia
    Galaxy,
    /// Efecto de agujero negro
    BlackHole,
    /// Efecto de aurora boreal
    Aurora,
    /// Efecto de cometa
    Comet,
    /// Efecto de supernova
    Supernova,
    /// Efecto de lente gravitacional
    GravitationalLens,
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
    pub progress: f32,           // 0.0 a 1.0
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
    pub fn activate_effect(
        &mut self,
        effect_id: &str,
        config: VisualEffectConfig,
    ) -> Result<(), String> {
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
            Err("Efecto no encontrado".to_string())
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
                    state.progress =
                        (state.current_time_ms as f32 / state.config.duration_ms as f32).min(1.0);
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
                    }
                    VisualEffectType::Glow => {
                        self.render_glow_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Shadow => {
                        self.render_shadow_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Blur => {
                        self.render_blur_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Transparency => {
                        self.render_transparency_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Animation => {
                        self.render_animation_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Gradient => {
                        self.render_gradient_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Reflection => {
                        self.render_reflection_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Distortion => {
                        self.render_distortion_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Lighting => {
                        self.render_lighting_effect(framebuffer, state)?;
                    }
                    VisualEffectType::SpaceParticles => {
                        self.render_space_particles_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Nebula => {
                        self.render_nebula_effect(framebuffer, state)?;
                    }
                    VisualEffectType::MovingStars => {
                        self.render_moving_stars_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Galaxy => {
                        self.render_galaxy_effect(framebuffer, state)?;
                    }
                    VisualEffectType::BlackHole => {
                        self.render_black_hole_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Aurora => {
                        self.render_aurora_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Comet => {
                        self.render_comet_effect(framebuffer, state)?;
                    }
                    VisualEffectType::Supernova => {
                        self.render_supernova_effect(framebuffer, state)?;
                    }
                    VisualEffectType::GravitationalLens => {
                        self.render_gravitational_lens_effect(framebuffer, state)?;
                    }
                }
                effects_rendered += 1;
            }
        }

        // Actualizar estadísticas
        self.update_performance_stats(effects_rendered, start_time);

        Ok(())
    }

    /// Renderizar efecto de partículas
    fn render_particle_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
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
    fn render_glow_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
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
    fn render_shadow_effect(
        &self,
        _framebuffer: &mut FramebufferDriver,
        _state: &VisualEffectState,
    ) -> Result<(), String> {
        // Simular efecto de sombra (en implementación real sería más complejo)
        Ok(())
    }

    /// Renderizar efecto de desenfoque
    fn render_blur_effect(
        &self,
        _framebuffer: &mut FramebufferDriver,
        _state: &VisualEffectState,
    ) -> Result<(), String> {
        // Simular efecto de blur (en implementación real sería más complejo)
        Ok(())
    }

    /// Renderizar efecto de transparencia
    fn render_transparency_effect(
        &self,
        _framebuffer: &mut FramebufferDriver,
        _state: &VisualEffectState,
    ) -> Result<(), String> {
        // Simular efecto de transparencia
        Ok(())
    }

    /// Renderizar efecto de animación
    fn render_animation_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
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
                let wave = sin((x as f32 * 0.1)
                    + (state.current_time_ms as f32 * animation_speed as f32 * 0.01));
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
    fn render_gradient_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
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
                if x % 8 == 0 {
                    // Renderizar cada 8 píxeles para optimizar
                    framebuffer.put_pixel(x, y, color);
                }
            }
        }

        Ok(())
    }

    /// Renderizar efecto de reflexión
    fn render_reflection_effect(
        &self,
        _framebuffer: &mut FramebufferDriver,
        _state: &VisualEffectState,
    ) -> Result<(), String> {
        // Simular efecto de reflexión
        Ok(())
    }

    /// Renderizar efecto de distorsión
    fn render_distortion_effect(
        &self,
        _framebuffer: &mut FramebufferDriver,
        _state: &VisualEffectState,
    ) -> Result<(), String> {
        // Simular efecto de distorsión
        Ok(())
    }

    /// Renderizar efecto de iluminación
    fn render_lighting_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;

        // Crear efecto de iluminación radial
        let center_x = width / 2;
        let center_y = height / 2;
        let max_distance =
            sqrt((center_x as f32 * center_x as f32 + center_y as f32 * center_y as f32) as f64);

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
            // === EFECTOS ESPACIALES ===
            VisualEffectType::SpaceParticles => 0.4,
            VisualEffectType::Nebula => 0.7,
            VisualEffectType::MovingStars => 0.3,
            VisualEffectType::Galaxy => 0.8,
            VisualEffectType::BlackHole => 0.9,
            VisualEffectType::Aurora => 0.6,
            VisualEffectType::Comet => 0.5,
            VisualEffectType::Supernova => 0.8,
            VisualEffectType::GravitationalLens => 0.9,
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
            }
            EffectQualityPreset::Balanced => {
                self.global_config.max_active_effects = 6;
                self.global_config.min_fps_threshold = 45.0;
            }
            EffectQualityPreset::Quality => {
                self.global_config.max_active_effects = 10;
                self.global_config.min_fps_threshold = 30.0;
            }
            EffectQualityPreset::Ultra => {
                self.global_config.max_active_effects = 15;
                self.global_config.min_fps_threshold = 20.0;
            }
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

    // === MÉTODOS DE RENDERIZADO DE EFECTOS ESPACIALES ===

    /// Renderizar efecto de partículas espaciales
    fn render_space_particles_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;
        let particle_count = match state.config.intensity {
            EffectIntensity::Low => 25,
            EffectIntensity::Medium => 50,
            EffectIntensity::High => 100,
            EffectIntensity::Ultra => 200,
        };

        for i in 0..particle_count {
            let x = ((state.current_time_ms + i * 13) % width as u32) as u32;
            let y = ((state.current_time_ms * 2 + i * 17) % height as u32) as u32;

            // Crear partículas con colores espaciales
            let particle_type = i % 4;
            let color = match particle_type {
                0 => Color::from_rgba(100, 150, 255, 200), // Azul espacial
                1 => Color::from_rgba(255, 200, 100, 180), // Dorado estelar
                2 => Color::from_rgba(200, 100, 255, 160), // Púrpura cósmico
                _ => Color::from_rgba(255, 255, 255, 140), // Blanco estelar
            };

            framebuffer.put_pixel(x, y, color);
        }

        Ok(())
    }

    /// Renderizar efecto de nebulosa
    fn render_nebula_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;

        // Crear gradiente nebuloso con colores cósmicos
        for y in 0..height {
            for x in 0..width {
                let center_x = width / 2;
                let center_y = height / 2;
                let dx = x as i32 - center_x as i32;
                let dy = y as i32 - center_y as i32;
                let distance = sqrt((dx * dx + dy * dy) as f64);

                let intensity =
                    (1.0 - if (distance / (width as f64 * 0.7)) < 1.0 {
                        distance / (width as f64 * 0.7)
                    } else {
                        1.0
                    }) * state.progress as f64;
                let alpha = (intensity * 100.0) as u8;

                // Colores de nebulosa
                let r = (intensity * 255.0 * 0.8) as u8;
                let g = (intensity * 255.0 * 0.4) as u8;
                let b = (intensity * 255.0 * 0.9) as u8;

                if alpha > 10 {
                    let color = Color::from_rgba(r, g, b, alpha);
                    framebuffer.put_pixel(x, y, color);
                }
            }
        }

        Ok(())
    }

    /// Renderizar efecto de estrellas en movimiento
    fn render_moving_stars_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;
        let star_count = match state.config.intensity {
            EffectIntensity::Low => 15,
            EffectIntensity::Medium => 30,
            EffectIntensity::High => 60,
            EffectIntensity::Ultra => 120,
        };

        for i in 0..star_count {
            // Simular movimiento de estrellas
            let speed = 1.0 + (i % 3) as f32 * 0.5;
            let x =
                ((state.current_time_ms as f32 * speed + i as f32 * 50.0) % width as f32) as u32;
            let y = ((state.current_time_ms as f32 * speed * 0.7 + i as f32 * 37.0) % height as f32)
                as u32;

            // Brillo variable de las estrellas
            let brightness =
                (sin((state.current_time_ms as f32 + i as f32 * 10.0) * 0.01) + 1.0) * 127.5;
            let star_color =
                Color::from_rgba(brightness as u8, brightness as u8, brightness as u8, 255);

            framebuffer.put_pixel(x, y, star_color);

            // Agregar destello ocasional
            if (state.current_time_ms + i * 7) % 100 < 5 {
                let flash_color = Color::from_rgba(255, 255, 255, 200);
                framebuffer.put_pixel(x, y, flash_color);
            }
        }

        Ok(())
    }

    /// Renderizar efecto de galaxia
    fn render_galaxy_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;

        // Crear espiral galáctica
        let center_x = width / 2;
        let center_y = height / 2;

        for i in 0..100 {
            let angle_f64 = (i as f64 * 0.1 + state.current_time_ms as f64 * 0.001) * 2.0 * PI;
            let angle_f32 = angle_f64 as f32;
            let radius = i as f32 * 2.0;

            let x = (center_x as f32 + radius * cos(angle_f64) as f32) as u32;
            let y = (center_y as f32 + radius * sin(angle_f32)) as u32;

            if x < width && y < height {
                let intensity = (1.0 - radius / 200.0).max(0.0);
                let brightness = (intensity * 255.0 * state.progress) as u8;

                let galaxy_color =
                    Color::from_rgba(brightness, brightness * 3 / 4, brightness / 2, brightness);
                framebuffer.put_pixel(x, y, galaxy_color);
            }
        }

        Ok(())
    }

    /// Renderizar efecto de agujero negro
    fn render_black_hole_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;

        let center_x = width / 2;
        let center_y = height / 2;

        // Renderizar horizonte de eventos
        for y in 0..height {
            for x in 0..width {
                let dx = x as i32 - center_x as i32;
                let dy = y as i32 - center_y as i32;
                let distance = sqrt((dx * dx + dy * dy) as f64);

                if distance < 30.0 {
                    // Agujero negro central
                    let color = Color::from_rgba(0, 0, 0, 255);
                    framebuffer.put_pixel(x, y, color);
                } else if distance < 50.0 {
                    // Horizonte de eventos
                    let intensity = ((50.0 - distance) / 20.0 * state.progress as f64) as u8;
                    let color =
                        Color::from_rgba(intensity, intensity / 2, intensity / 4, intensity);
                    framebuffer.put_pixel(x, y, color);
                } else if distance < 80.0 {
                    // Disco de acreción
                    let intensity = ((80.0 - distance) / 30.0 * state.progress as f64 * 0.5) as u8;
                    let color = Color::from_rgba(intensity * 2, intensity, 0, intensity);
                    framebuffer.put_pixel(x, y, color);
                }
            }
        }

        Ok(())
    }

    /// Renderizar efecto de aurora boreal
    fn render_aurora_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;

        // Crear ondas de aurora
        for y in 0..height {
            for x in 0..width {
                let wave1 = sin((x as f32 * 0.02 + state.current_time_ms as f32 * 0.005)) * 20.0;
                let wave2 = sin((x as f32 * 0.03 + state.current_time_ms as f32 * 0.003)) * 15.0;
                let wave3 = sin((x as f32 * 0.04 + state.current_time_ms as f32 * 0.007)) * 25.0;

                let combined_wave = wave1 + wave2 + wave3;
                let aurora_y = height as f32 / 2.0 + combined_wave;

                if (y as f32 - aurora_y).abs() < 10.0 {
                    let intensity = (1.0 - (y as f32 - aurora_y).abs() / 10.0) * state.progress;
                    let alpha = (intensity * 200.0) as u8;

                    // Colores de aurora (verde y azul)
                    let green = (intensity * 255.0) as u8;
                    let blue = (intensity * 180.0) as u8;
                    let red = (intensity * 50.0) as u8;

                    let color = Color::from_rgba(red, green, blue, alpha);
                    framebuffer.put_pixel(x, y, color);
                }
            }
        }

        Ok(())
    }

    /// Renderizar efecto de cometa
    fn render_comet_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;

        // Posición del cometa
        let comet_x = ((state.current_time_ms as f32 * 0.5) % width as f32) as u32;
        let comet_y =
            (height / 2) as i32 + (sin(state.current_time_ms as f32 * 0.002) * 50.0) as i32;

        if comet_x < width && comet_y >= 0 && comet_y < height as i32 {
            // Núcleo del cometa
            let core_color = Color::from_rgba(255, 255, 255, 255);
            framebuffer.put_pixel(comet_x, comet_y as u32, core_color);

            // Cola del cometa
            for i in 1..20 {
                let tail_x = comet_x.saturating_sub(i);
                let tail_y = comet_y as u32;

                if tail_x < width {
                    let intensity = (1.0 - i as f32 / 20.0) * state.progress;
                    let alpha = (intensity * 150.0) as u8;
                    let blue = (intensity * 255.0) as u8;

                    let tail_color = Color::from_rgba(100, 150, blue, alpha);
                    framebuffer.put_pixel(tail_x, tail_y, tail_color);
                }
            }
        }

        Ok(())
    }

    /// Renderizar efecto de supernova
    fn render_supernova_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;

        let center_x = width / 2;
        let center_y = height / 2;
        let explosion_radius = (state.progress * 100.0) as f32;

        for y in 0..height {
            for x in 0..width {
                let dx = x as i32 - center_x as i32;
                let dy = y as i32 - center_y as i32;
                let distance = sqrt((dx * dx + dy * dy) as f64);

                if distance <= explosion_radius as f64 {
                    let intensity =
                        (1.0 - distance / explosion_radius as f64) * state.progress as f64;

                    if intensity > 0.1 {
                        // Colores de supernova (amarillo, naranja, rojo)
                        let red = (intensity * 255.0) as u8;
                        let green = (intensity * 200.0) as u8;
                        let blue = (intensity * 100.0) as u8;
                        let alpha = (intensity * 255.0) as u8;

                        let supernova_color = Color::from_rgba(red, green, blue, alpha);
                        framebuffer.put_pixel(x, y, supernova_color);
                    }
                }
            }
        }

        Ok(())
    }

    /// Renderizar efecto de lente gravitacional
    fn render_gravitational_lens_effect(
        &self,
        framebuffer: &mut FramebufferDriver,
        state: &VisualEffectState,
    ) -> Result<(), String> {
        let info = framebuffer.get_info();
        let width = info.width;
        let height = info.height;

        let center_x = width / 2;
        let center_y = height / 2;

        // Crear distorsión gravitacional
        for y in 0..height {
            for x in 0..width {
                let dx = x as i32 - center_x as i32;
                let dy = y as i32 - center_y as i32;
                let distance = sqrt((dx * dx + dy * dy) as f64);

                if distance > 20.0 && distance < 80.0 {
                    // Aplicar distorsión gravitacional
                    let distortion_factor = (1.0 + 30.0 / distance) * state.progress as f64;

                    let distorted_x =
                        (center_x as f32 + dx as f32 * distortion_factor as f32) as u32;
                    let distorted_y =
                        (center_y as f32 + dy as f32 * distortion_factor as f32) as u32;

                    if distorted_x < width && distorted_y < height {
                        let intensity = ((80.0 - distance) / 60.0 * state.progress as f64) as u8;
                        let lens_color =
                            Color::from_rgba(intensity, intensity, intensity * 2, intensity);
                        framebuffer.put_pixel(distorted_x, distorted_y, lens_color);
                    }
                }
            }
        }

        Ok(())
    }
}
