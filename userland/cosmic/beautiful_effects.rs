//! Efectos Visuales Hermosos para COSMIC
//!
//! Este módulo implementa efectos visuales avanzados y hermosos
//! para hacer COSMIC más atractivo visualmente.

// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use crate::math_utils::{sin, sqrt};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Sistema de efectos visuales hermosos
#[derive(Debug, Clone)]
pub struct BeautifulEffects {
    /// Efectos de partículas activos
    pub particles: Vec<Particle>,
    /// Efectos de gradientes
    pub gradients: Vec<GradientEffect>,
    /// Efectos de animación
    pub animations: Vec<AnimationEffect>,
    /// Efectos de iluminación
    pub lighting: Vec<LightingEffect>,
    /// Contador de frames para animaciones
    pub frame_count: u32,
    /// Configuración de efectos
    pub config: EffectsConfig,
}

/// Partícula individual
#[derive(Debug, Clone)]
pub struct Particle {
    pub x: f32,
    pub y: f32,
    pub velocity_x: f32,
    pub velocity_y: f32,
    pub size: f32,
    pub color: Color,
    pub life: f32,
    pub max_life: f32,
    pub particle_type: ParticleType,
}

/// Tipo de partícula
#[derive(Debug, Clone, Copy)]
pub enum ParticleType {
    Star,
    Sparkle,
    Glow,
    Trail,
    Bubble,
}

/// Efecto de gradiente
#[derive(Debug, Clone)]
pub struct GradientEffect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub start_color: Color,
    pub end_color: Color,
    pub direction: GradientDirection,
    pub animated: bool,
    pub animation_speed: f32,
}

/// Dirección del gradiente
#[derive(Debug, Clone, Copy)]
pub enum GradientDirection {
    Horizontal,
    Vertical,
    Diagonal,
    Radial,
}

/// Efecto de animación
#[derive(Debug, Clone)]
pub struct AnimationEffect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub animation_type: AnimationType,
    pub progress: f32,
    pub speed: f32,
    pub color: Color,
}

/// Tipo de animación
#[derive(Debug, Clone, Copy)]
pub enum AnimationType {
    FadeIn,
    FadeOut,
    SlideIn,
    SlideOut,
    ScaleIn,
    ScaleOut,
    Pulse,
    Rotate,
    Wave,
    Glow,
}

/// Efecto de iluminación
#[derive(Debug, Clone)]
pub struct LightingEffect {
    pub x: u32,
    pub y: u32,
    pub radius: u32,
    pub intensity: f32,
    pub color: Color,
    pub animated: bool,
    pub animation_speed: f32,
}

/// Configuración de efectos
#[derive(Debug, Clone)]
pub struct EffectsConfig {
    pub enable_particles: bool,
    pub enable_gradients: bool,
    pub enable_animations: bool,
    pub enable_lighting: bool,
    pub particle_count: u32,
    pub animation_speed: f32,
    pub quality_level: f32, // 0.0 a 1.0
}

impl BeautifulEffects {
    /// Crear nuevo sistema de efectos hermosos
    pub fn new() -> Self {
        Self {
            particles: Vec::new(),
            gradients: Vec::new(),
            animations: Vec::new(),
            lighting: Vec::new(),
            frame_count: 0,
            config: EffectsConfig {
                enable_particles: true,
                enable_gradients: true,
                enable_animations: true,
                enable_lighting: true,
                particle_count: 50,
                animation_speed: 1.0,
                quality_level: 0.8,
            },
        }
    }

    /// Crear con configuración personalizada
    pub fn with_config(config: EffectsConfig) -> Self {
        Self {
            particles: Vec::new(),
            gradients: Vec::new(),
            animations: Vec::new(),
            lighting: Vec::new(),
            frame_count: 0,
            config,
        }
    }

    /// Actualizar todos los efectos
    pub fn update(&mut self, delta_time: f32) {
        self.frame_count += 1;

        if self.config.enable_particles {
            self.update_particles(delta_time);
        }

        if self.config.enable_animations {
            self.update_animations(delta_time);
        }

        if self.config.enable_lighting {
            self.update_lighting(delta_time);
        }
    }

    /// Renderizar todos los efectos
    pub fn render(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        if self.config.enable_gradients {
            self.render_gradients(fb)?;
        }

        if self.config.enable_lighting {
            self.render_lighting(fb)?;
        }

        if self.config.enable_particles {
            self.render_particles(fb)?;
        }

        if self.config.enable_animations {
            self.render_animations(fb)?;
        }

        Ok(())
    }

    /// Actualizar partículas
    fn update_particles(&mut self, delta_time: f32) {
        // Actualizar partículas existentes
        self.particles.retain_mut(|particle| {
            particle.x += particle.velocity_x * delta_time;
            particle.y += particle.velocity_y * delta_time;
            particle.life -= delta_time;

            // Aplicar gravedad a ciertos tipos de partículas
            match particle.particle_type {
                ParticleType::Bubble => {
                    particle.velocity_y -= 0.5 * delta_time; // Flotar hacia arriba
                }
                ParticleType::Trail => {
                    particle.velocity_y += 0.2 * delta_time; // Caer hacia abajo
                }
                _ => {}
            }

            particle.life > 0.0
        });

        // Generar nuevas partículas si es necesario
        if self.particles.len() < self.config.particle_count as usize {
            self.generate_random_particles(5);
        }
    }

    /// Actualizar animaciones
    fn update_animations(&mut self, delta_time: f32) {
        for animation in &mut self.animations {
            animation.progress += animation.speed * delta_time * self.config.animation_speed;

            // Reiniciar animación si ha terminado
            if animation.progress > 1.0 {
                animation.progress = 0.0;
            }
        }
    }

    /// Actualizar efectos de iluminación
    fn update_lighting(&mut self, delta_time: f32) {
        for light in &mut self.lighting {
            if light.animated {
                // Simular animación de intensidad simple
                light.intensity = 0.5
                    + 0.5 * sin((self.frame_count as f32 * light.animation_speed * 0.01) % 6.28);
            }
        }
    }

    /// Renderizar gradientes
    fn render_gradients(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        for gradient in &self.gradients {
            self.render_single_gradient(fb, gradient)?;
        }
        Ok(())
    }

    /// Renderizar un gradiente individual
    fn render_single_gradient(
        &self,
        fb: &mut FramebufferDriver,
        gradient: &GradientEffect,
    ) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;

        for y in gradient.y..(gradient.y + gradient.height) {
            for x in gradient.x..(gradient.x + gradient.width) {
                if x < width && y < height {
                    let color = self.calculate_gradient_color(gradient, x, y);
                    fb.put_pixel(x, y, color);
                }
            }
        }
        Ok(())
    }

    /// Calcular color de gradiente
    fn calculate_gradient_color(&self, gradient: &GradientEffect, x: u32, y: u32) -> Color {
        let progress = match gradient.direction {
            GradientDirection::Horizontal => {
                if gradient.width > 0 {
                    (x - gradient.x) as f32 / gradient.width as f32
                } else {
                    0.0
                }
            }
            GradientDirection::Vertical => {
                if gradient.height > 0 {
                    (y - gradient.y) as f32 / gradient.height as f32
                } else {
                    0.0
                }
            }
            GradientDirection::Diagonal => {
                let h_progress = if gradient.width > 0 {
                    (x - gradient.x) as f32 / gradient.width as f32
                } else {
                    0.0
                };
                let v_progress = if gradient.height > 0 {
                    (y - gradient.y) as f32 / gradient.height as f32
                } else {
                    0.0
                };
                (h_progress + v_progress) / 2.0
            }
            GradientDirection::Radial => {
                let center_x = gradient.x + gradient.width / 2;
                let center_y = gradient.y + gradient.height / 2;
                let dx = x as i32 - center_x as i32;
                let dy = y as i32 - center_y as i32;
                let distance_squared = (dx * dx + dy * dy) as f32;
                let max_distance_squared =
                    ((gradient.width / 2).pow(2) + (gradient.height / 2).pow(2)) as f32;
                if max_distance_squared > 0.0 {
                    (distance_squared / max_distance_squared).min(1.0)
                } else {
                    0.0
                }
            }
        };

        self.interpolate_colors(&gradient.start_color, &gradient.end_color, progress)
    }

    /// Interpolar entre dos colores
    fn interpolate_colors(&self, start: &Color, end: &Color, progress: f32) -> Color {
        let progress = progress.max(0.0).min(1.0);

        // Interpolación simple basada en el progreso
        if progress < 0.5 {
            *start
        } else {
            *end
        }
    }

    /// Renderizar partículas
    fn render_particles(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        for particle in &self.particles {
            self.render_single_particle(fb, particle)?;
        }
        Ok(())
    }

    /// Renderizar una partícula individual
    fn render_single_particle(
        &self,
        fb: &mut FramebufferDriver,
        particle: &Particle,
    ) -> Result<(), String> {
        let x = particle.x as u32;
        let y = particle.y as u32;
        let size = particle.size as u32;

        // Renderizar partícula según su tipo
        match particle.particle_type {
            ParticleType::Star => {
                self.render_star_particle(fb, x, y, size, &particle.color)?;
            }
            ParticleType::Sparkle => {
                self.render_sparkle_particle(fb, x, y, size, &particle.color)?;
            }
            ParticleType::Glow => {
                self.render_glow_particle(fb, x, y, size, &particle.color)?;
            }
            ParticleType::Trail => {
                self.render_trail_particle(fb, x, y, size, &particle.color)?;
            }
            ParticleType::Bubble => {
                self.render_bubble_particle(fb, x, y, size, &particle.color)?;
            }
        }
        Ok(())
    }

    /// Renderizar partícula estrella
    fn render_star_particle(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        size: u32,
        color: &Color,
    ) -> Result<(), String> {
        if size == 0 {
            return Ok(());
        }

        // Dibujar estrella simple
        for dy in 0..size {
            for dx in 0..size {
                let px = x + dx;
                let py = y + dy;

                if px < fb.info.width && py < fb.info.height {
                    // Patrón de estrella simple
                    let center = size / 2;
                    let distance_squared =
                        (dx as i32 - center as i32).pow(2) + (dy as i32 - center as i32).pow(2);

                    if distance_squared < (size as i32 / 2).pow(2) {
                        fb.put_pixel(px, py, *color);
                    }
                }
            }
        }
        Ok(())
    }

    /// Renderizar partícula brillante
    fn render_sparkle_particle(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        size: u32,
        color: &Color,
    ) -> Result<(), String> {
        if size == 0 {
            return Ok(());
        }

        // Dibujar cruz brillante
        let center = size / 2;
        for i in 0..size {
            if x + i < fb.info.width && y + center < fb.info.height {
                fb.put_pixel(x + i, y + center, *color);
            }
            if x + center < fb.info.width && y + i < fb.info.height {
                fb.put_pixel(x + center, y + i, *color);
            }
        }
        Ok(())
    }

    /// Renderizar partícula de resplandor
    fn render_glow_particle(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        size: u32,
        color: &Color,
    ) -> Result<(), String> {
        if size == 0 {
            return Ok(());
        }

        // Dibujar círculo con resplandor
        let center = size / 2;
        for dy in 0..size {
            for dx in 0..size {
                let px = x + dx;
                let py = y + dy;

                if px < fb.info.width && py < fb.info.height {
                    let distance_squared =
                        (dx as i32 - center as i32).pow(2) + (dy as i32 - center as i32).pow(2);

                    if distance_squared < (size as i32 / 2).pow(2) {
                        fb.put_pixel(px, py, *color);
                    }
                }
            }
        }
        Ok(())
    }

    /// Renderizar partícula de rastro
    fn render_trail_particle(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        size: u32,
        color: &Color,
    ) -> Result<(), String> {
        if size == 0 {
            return Ok(());
        }

        // Dibujar línea vertical
        for i in 0..size {
            if x < fb.info.width && y + i < fb.info.height {
                fb.put_pixel(x, y + i, *color);
            }
        }
        Ok(())
    }

    /// Renderizar partícula de burbuja
    fn render_bubble_particle(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        size: u32,
        color: &Color,
    ) -> Result<(), String> {
        if size == 0 {
            return Ok(());
        }

        // Dibujar círculo hueco
        let center = size / 2;
        for dy in 0..size {
            for dx in 0..size {
                let px = x + dx;
                let py = y + dy;

                if px < fb.info.width && py < fb.info.height {
                    let distance_squared =
                        (dx as i32 - center as i32).pow(2) + (dy as i32 - center as i32).pow(2);
                    let radius_squared = (size as i32 / 2).pow(2);

                    if distance_squared >= radius_squared - 1 && distance_squared <= radius_squared
                    {
                        fb.put_pixel(px, py, *color);
                    }
                }
            }
        }
        Ok(())
    }

    /// Renderizar animaciones
    fn render_animations(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        for animation in &self.animations {
            self.render_single_animation(fb, animation)?;
        }
        Ok(())
    }

    /// Renderizar una animación individual
    fn render_single_animation(
        &self,
        fb: &mut FramebufferDriver,
        animation: &AnimationEffect,
    ) -> Result<(), String> {
        // Dibujar rectángulo animado
        for y in animation.y..(animation.y + animation.height) {
            for x in animation.x..(animation.x + animation.width) {
                if x < fb.info.width && y < fb.info.height {
                    fb.put_pixel(x, y, animation.color);
                }
            }
        }
        Ok(())
    }

    /// Renderizar efectos de iluminación
    fn render_lighting(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        for light in &self.lighting {
            self.render_single_light(fb, light)?;
        }
        Ok(())
    }

    /// Renderizar una luz individual
    fn render_single_light(
        &self,
        fb: &mut FramebufferDriver,
        light: &LightingEffect,
    ) -> Result<(), String> {
        let width = fb.info.width;
        let height = fb.info.height;

        for y in 0..height {
            for x in 0..width {
                let dx = x as i32 - light.x as i32;
                let dy = y as i32 - light.y as i32;
                let distance_squared = dx * dx + dy * dy;

                if distance_squared <= (light.radius as i32).pow(2) {
                    // Simplificar cálculo de intensidad
                    let intensity = light.intensity;
                    if intensity > 0.1 {
                        fb.put_pixel(x, y, light.color);
                    }
                }
            }
        }
        Ok(())
    }

    /// Generar partículas aleatorias
    fn generate_random_particles(&mut self, count: u32) {
        for _ in 0..count {
            let particle = Particle {
                x: (self.frame_count * 7) as f32 % 800.0,
                y: (self.frame_count * 11) as f32 % 600.0,
                velocity_x: ((self.frame_count * 13) as f32 % 100.0 - 50.0) / 10.0,
                velocity_y: ((self.frame_count * 17) as f32 % 100.0 - 50.0) / 10.0,
                size: 2.0 + ((self.frame_count * 19) as f32 % 3.0),
                color: self.get_random_color(),
                life: 2.0 + ((self.frame_count * 23) as f32 % 3.0),
                max_life: 5.0,
                particle_type: self.get_random_particle_type(),
            };
            self.particles.push(particle);
        }
    }

    /// Obtener color aleatorio
    fn get_random_color(&self) -> Color {
        let colors = [
            Color::WHITE,
            Color::YELLOW,
            Color::CYAN,
            Color::MAGENTA,
            Color::GREEN,
            Color::BLUE,
        ];
        colors[(self.frame_count as usize) % colors.len()]
    }

    /// Obtener tipo de partícula aleatorio
    fn get_random_particle_type(&self) -> ParticleType {
        let types = [
            ParticleType::Star,
            ParticleType::Sparkle,
            ParticleType::Glow,
            ParticleType::Trail,
            ParticleType::Bubble,
        ];
        types[(self.frame_count as usize) % types.len()]
    }

    /// Agregar efecto de gradiente
    pub fn add_gradient(&mut self, gradient: GradientEffect) {
        self.gradients.push(gradient);
    }

    /// Agregar efecto de animación
    pub fn add_animation(&mut self, animation: AnimationEffect) {
        self.animations.push(animation);
    }

    /// Agregar efecto de iluminación
    pub fn add_lighting(&mut self, lighting: LightingEffect) {
        self.lighting.push(lighting);
    }

    /// Crear fondo hermoso
    pub fn create_beautiful_background(&mut self, width: u32, height: u32) {
        // Gradiente de fondo espacial
        self.gradients.push(GradientEffect {
            x: 0,
            y: 0,
            width,
            height,
            start_color: Color::DARK_BLUE,
            end_color: Color::BLACK,
            direction: GradientDirection::Vertical,
            animated: false,
            animation_speed: 0.0,
        });

        // Efectos de iluminación
        self.lighting.push(LightingEffect {
            x: width / 4,
            y: height / 4,
            radius: 100,
            intensity: 0.3,
            color: Color::CYAN,
            animated: true,
            animation_speed: 0.02,
        });

        self.lighting.push(LightingEffect {
            x: 3 * width / 4,
            y: 3 * height / 4,
            radius: 80,
            intensity: 0.2,
            color: Color::MAGENTA,
            animated: true,
            animation_speed: 0.015,
        });
    }

    /// Crear efectos de bienvenida
    pub fn create_welcome_effects(&mut self, x: u32, y: u32, width: u32, height: u32) {
        // Animación de aparición
        self.animations.push(AnimationEffect {
            x,
            y,
            width,
            height,
            animation_type: AnimationType::FadeIn,
            progress: 0.0,
            speed: 0.02,
            color: Color::WHITE,
        });

        // Efecto de pulso
        self.animations.push(AnimationEffect {
            x: x + width / 4,
            y: y + height / 4,
            width: width / 2,
            height: height / 2,
            animation_type: AnimationType::Pulse,
            progress: 0.0,
            speed: 0.03,
            color: Color::YELLOW,
        });
    }

    /// Obtener estadísticas de efectos
    pub fn get_effects_stats(&self) -> String {
        format!(
            "Efectos: {} partículas, {} gradientes, {} animaciones, {} luces",
            self.particles.len(),
            self.gradients.len(),
            self.animations.len(),
            self.lighting.len()
        )
    }
}
