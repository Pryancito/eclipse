//! Efectos visuales espaciales avanzados para COSMIC Desktop Environment
//!
//! Implementa efectos visuales espectaculares: partículas, nebulosa, estrellas,
//! agujeros negros, aurora, supernova y más efectos espaciales.

// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// Funciones trigonométricas simples para no_std
mod math {
    /// Aproximación simple de seno
    pub fn sin(x: f32) -> f32 {
        // Aproximación usando la serie de Taylor truncada
        let x = x % 6.28318530718; // Normalizar a [0, 2π]
        let x2 = x * x;
        let x3 = x2 * x;
        let x5 = x3 * x2;
        let x7 = x5 * x2;

        x - x3 / 6.0 + x5 / 120.0 - x7 / 5040.0
    }

    /// Aproximación simple de coseno
    pub fn cos(x: f32) -> f32 {
        // cos(x) = sin(x + π/2)
        sin(x + 1.57079632679)
    }
}

/// Tipo de partícula espacial
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParticleType {
    Star,
    Nebula,
    Comet,
    Meteor,
    Supernova,
    BlackHole,
    Aurora,
    CosmicDust,
}

/// Partícula espacial
#[derive(Debug, Clone)]
pub struct SpaceParticle {
    pub particle_type: ParticleType,
    pub x: f32,
    pub y: f32,
    pub velocity_x: f32,
    pub velocity_y: f32,
    pub size: f32,
    pub color: Color,
    pub alpha: f32,
    pub life: f32,
    pub max_life: f32,
    pub rotation: f32,
    pub rotation_speed: f32,
    pub scale: f32,
    pub glow_intensity: f32,
}

/// Efecto de nebulosa
#[derive(Debug, Clone)]
pub struct NebulaEffect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub color1: Color,
    pub color2: Color,
    pub color3: Color,
    pub animation_phase: f32,
    pub rotation: f32,
    pub scale: f32,
    pub opacity: f32,
}

/// Efecto de agujero negro
#[derive(Debug, Clone)]
pub struct BlackHoleEffect {
    pub x: f32,
    pub y: f32,
    pub radius: f32,
    pub event_horizon: f32,
    pub accretion_disk: Vec<SpaceParticle>,
    pub lens_effect_strength: f32,
    pub rotation: f32,
    pub gravity_pull: f32,
}

/// Efecto de aurora
#[derive(Debug, Clone)]
pub struct AuroraEffect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub wave_phase: f32,
    pub color_shift: f32,
    pub intensity: f32,
    pub wave_frequency: f32,
    pub wave_amplitude: f32,
}

/// Efecto de supernova
#[derive(Debug, Clone)]
pub struct SupernovaEffect {
    pub x: f32,
    pub y: f32,
    pub explosion_radius: f32,
    pub max_radius: f32,
    pub shockwave_particles: Vec<SpaceParticle>,
    pub light_flash_intensity: f32,
    pub animation_phase: f32,
    pub is_active: bool,
}

/// Configuración de efectos visuales
#[derive(Debug, Clone)]
pub struct VisualEffectsConfig {
    pub particle_count: usize,
    pub nebula_count: usize,
    pub star_count: usize,
    pub comet_frequency: f32,
    pub meteor_frequency: f32,
    pub supernova_frequency: f32,
    pub animation_speed: f32,
    pub color_intensity: f32,
    pub glow_enabled: bool,
    pub parallax_enabled: bool,
}

/// Gestor de efectos visuales avanzados
pub struct AdvancedVisualEffects {
    particles: Vec<SpaceParticle>,
    nebulas: Vec<NebulaEffect>,
    black_holes: Vec<BlackHoleEffect>,
    auroras: Vec<AuroraEffect>,
    supernovas: Vec<SupernovaEffect>,
    config: VisualEffectsConfig,
    time: f32,
    screen_width: u32,
    screen_height: u32,
    particle_counter: u32,
}

impl AdvancedVisualEffects {
    pub fn new(screen_width: u32, screen_height: u32) -> Self {
        let config = VisualEffectsConfig {
            particle_count: 200,
            nebula_count: 3,
            star_count: 150,
            comet_frequency: 0.001,
            meteor_frequency: 0.002,
            supernova_frequency: 0.0005,
            animation_speed: 1.0,
            color_intensity: 1.0,
            glow_enabled: true,
            parallax_enabled: true,
        };

        let mut effects = Self {
            particles: Vec::new(),
            nebulas: Vec::new(),
            black_holes: Vec::new(),
            auroras: Vec::new(),
            supernovas: Vec::new(),
            config,
            time: 0.0,
            screen_width,
            screen_height,
            particle_counter: 0,
        };

        effects.initialize_effects();
        effects
    }

    /// Inicializar todos los efectos
    fn initialize_effects(&mut self) {
        self.create_stars();
        self.create_nebulas();
        self.create_black_holes();
        self.create_auroras();
    }

    /// Crear estrellas de fondo
    fn create_stars(&mut self) {
        for _ in 0..self.config.star_count {
            let star = SpaceParticle {
                particle_type: ParticleType::Star,
                x: (self.particle_counter as f32 * 7.3) % self.screen_width as f32,
                y: (self.particle_counter as f32 * 11.7) % self.screen_height as f32,
                velocity_x: 0.0,
                velocity_y: 0.0,
                size: 1.0 + (self.particle_counter % 3) as f32,
                color: self.get_star_color(),
                alpha: 0.6 + (self.particle_counter % 4) as f32 * 0.1,
                life: 1.0,
                max_life: 1.0,
                rotation: 0.0,
                rotation_speed: 0.0,
                scale: 1.0,
                glow_intensity: 0.3 + (self.particle_counter % 3) as f32 * 0.2,
            };
            self.particles.push(star);
            self.particle_counter += 1;
        }
    }

    /// Crear nebulosas
    fn create_nebulas(&mut self) {
        for i in 0..self.config.nebula_count {
            let nebula = NebulaEffect {
                x: (i as f32 * 400.0) % self.screen_width as f32,
                y: (i as f32 * 300.0) % self.screen_height as f32,
                width: 200.0 + (i as f32 * 100.0),
                height: 150.0 + (i as f32 * 80.0),
                color1: self.get_nebula_color_1(i),
                color2: self.get_nebula_color_2(i),
                color3: self.get_nebula_color_3(i),
                animation_phase: i as f32 * 2.0,
                rotation: i as f32 * 45.0,
                scale: 0.8 + (i as f32 * 0.2),
                opacity: 0.3 + (i as f32 * 0.1),
            };
            self.nebulas.push(nebula);
        }
    }

    /// Crear agujeros negros
    fn create_black_holes(&mut self) {
        let black_hole = BlackHoleEffect {
            x: self.screen_width as f32 * 0.7,
            y: self.screen_height as f32 * 0.3,
            radius: 30.0,
            event_horizon: 50.0,
            accretion_disk: Vec::new(),
            lens_effect_strength: 0.8,
            rotation: 0.0,
            gravity_pull: 0.5,
        };
        self.black_holes.push(black_hole);
    }

    /// Crear auroras
    fn create_auroras(&mut self) {
        let aurora = AuroraEffect {
            x: 0.0,
            y: 50.0,
            width: self.screen_width as f32,
            height: 100.0,
            wave_phase: 0.0,
            color_shift: 0.0,
            intensity: 0.6,
            wave_frequency: 0.02,
            wave_amplitude: 30.0,
        };
        self.auroras.push(aurora);
    }

    /// Actualizar todos los efectos
    pub fn update(&mut self, delta_time: f32) {
        self.time += delta_time * self.config.animation_speed;

        self.update_particles(delta_time);
        self.update_nebulas(delta_time);
        self.update_black_holes(delta_time);
        self.update_auroras(delta_time);
        self.update_supernovas(delta_time);

        // Crear nuevos efectos ocasionales
        self.spawn_occasional_effects();
    }

    /// Actualizar partículas
    fn update_particles(&mut self, delta_time: f32) {
        let mut i = 0;
        while i < self.particles.len() {
            let particle = &mut self.particles[i];

            // Actualizar posición
            particle.x += particle.velocity_x * delta_time;
            particle.y += particle.velocity_y * delta_time;
            particle.rotation += particle.rotation_speed * delta_time;

            // Actualizar vida
            particle.life -= delta_time * 0.1;
            particle.alpha = (particle.life / particle.max_life).max(0.0);

            // Aplicar efectos específicos por tipo
            match particle.particle_type {
                ParticleType::Star => {
                    // Estrellas titilan
                    particle.glow_intensity =
                        0.3 + math::sin(self.time * 2.0 + particle.x * 0.01) * 0.3;
                }
                ParticleType::Comet => {
                    // Cometas se desvanecen con el tiempo
                    particle.size *= 0.999;
                    particle.velocity_x *= 0.998;
                    particle.velocity_y *= 0.998;
                }
                ParticleType::Meteor => {
                    // Meteoros se desintegran
                    particle.size *= 0.995;
                    particle.alpha *= 0.99;
                }
                ParticleType::Supernova => {
                    // Partículas de supernova se expanden
                    particle.velocity_x *= 1.01;
                    particle.velocity_y *= 1.01;
                    particle.size *= 1.005;
                }
                _ => {}
            }

            // Eliminar partículas muertas o fuera de pantalla
            if particle.life <= 0.0
                || particle.x < -50.0
                || particle.x > self.screen_width as f32 + 50.0
                || particle.y < -50.0
                || particle.y > self.screen_height as f32 + 50.0
            {
                self.particles.remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// Actualizar nebulosas
    fn update_nebulas(&mut self, delta_time: f32) {
        for nebula in &mut self.nebulas {
            nebula.animation_phase += delta_time * 0.5;
            nebula.rotation += delta_time * 10.0;
            nebula.scale = 0.8 + math::sin(nebula.animation_phase * 0.1) * 0.2;
            nebula.opacity = 0.3 + math::sin(nebula.animation_phase * 0.3) * 0.1;
        }
    }

    /// Actualizar agujeros negros
    fn update_black_holes(&mut self, delta_time: f32) {
        for black_hole in &mut self.black_holes {
            black_hole.rotation += delta_time * 20.0;

            // Crear partículas del disco de acreción
            if self.particles.len() < self.config.particle_count {
                let angle = black_hole.rotation * 0.1;
                let distance = black_hole.radius + 20.0 + math::sin(self.time * 0.5) * 10.0;

                let particle = SpaceParticle {
                    particle_type: ParticleType::CosmicDust,
                    x: black_hole.x + math::cos(angle) * distance,
                    y: black_hole.y + math::sin(angle) * distance,
                    velocity_x: -math::sin(angle) * 30.0,
                    velocity_y: math::cos(angle) * 30.0,
                    size: 2.0,
                    color: Color::from_hex(0xffaa00),
                    alpha: 0.8,
                    life: 5.0,
                    max_life: 5.0,
                    rotation: 0.0,
                    rotation_speed: 0.0,
                    scale: 1.0,
                    glow_intensity: 0.5,
                };
                self.particles.push(particle);
            }
        }
    }

    /// Actualizar auroras
    fn update_auroras(&mut self, delta_time: f32) {
        for aurora in &mut self.auroras {
            aurora.wave_phase += delta_time * aurora.wave_frequency;
            aurora.color_shift += delta_time * 0.3;
            aurora.intensity = 0.4 + math::sin(aurora.wave_phase * 0.5) * 0.3;
        }
    }

    /// Actualizar supernovas
    fn update_supernovas(&mut self, delta_time: f32) {
        let mut i = 0;
        while i < self.supernovas.len() {
            let supernova = &mut self.supernovas[i];

            if supernova.is_active {
                supernova.animation_phase += delta_time * 2.0;
                supernova.explosion_radius += delta_time * 100.0;
                supernova.light_flash_intensity = (1.0 - supernova.animation_phase).max(0.0);

                // Crear partículas de explosión
                if supernova.animation_phase < 0.5 {
                    for _ in 0..3 {
                        let angle = (self.time * 10.0 + supernova.animation_phase * 20.0) % 6.28;
                        let speed = 50.0 + supernova.animation_phase * 100.0;

                        let particle = SpaceParticle {
                            particle_type: ParticleType::Supernova,
                            x: supernova.x,
                            y: supernova.y,
                            velocity_x: math::cos(angle) * speed,
                            velocity_y: math::sin(angle) * speed,
                            size: 3.0 + supernova.animation_phase * 2.0,
                            color: Color::from_hex(0xffff00),
                            alpha: 1.0 - supernova.animation_phase,
                            life: 1.0 - supernova.animation_phase,
                            max_life: 1.0,
                            rotation: 0.0,
                            rotation_speed: 0.0,
                            scale: 1.0,
                            glow_intensity: 0.8,
                        };
                        self.particles.push(particle);
                    }
                }

                if supernova.animation_phase >= 1.0 {
                    supernova.is_active = false;
                }
            }

            if !supernova.is_active {
                self.supernovas.remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// Crear efectos ocasionales
    fn spawn_occasional_effects(&mut self) {
        // Crear cometas ocasionales
        if self.time % 100.0 < 0.1 && self.particles.len() < self.config.particle_count {
            self.create_comet();
        }

        // Crear meteoros ocasionales
        if self.time % 50.0 < 0.1 && self.particles.len() < self.config.particle_count {
            self.create_meteor();
        }

        // Crear supernovas ocasionales
        if self.time % 200.0 < 0.1 {
            self.create_supernova();
        }
    }

    /// Crear cometa
    fn create_comet(&mut self) {
        let comet = SpaceParticle {
            particle_type: ParticleType::Comet,
            x: -50.0,
            y: (self.time * 0.7) % self.screen_height as f32,
            velocity_x: 80.0 + math::sin(self.time * 0.1) * 20.0,
            velocity_y: math::sin(self.time * 0.3) * 10.0,
            size: 4.0,
            color: Color::from_hex(0x88ccff),
            alpha: 0.9,
            life: 8.0,
            max_life: 8.0,
            rotation: 0.0,
            rotation_speed: 0.0,
            scale: 1.0,
            glow_intensity: 0.7,
        };
        self.particles.push(comet);
    }

    /// Crear meteoro
    fn create_meteor(&mut self) {
        let meteor = SpaceParticle {
            particle_type: ParticleType::Meteor,
            x: (self.time * 0.5) % self.screen_width as f32,
            y: -20.0,
            velocity_x: math::sin(self.time * 0.2) * 15.0,
            velocity_y: 60.0 + math::cos(self.time * 0.4) * 10.0,
            size: 2.0,
            color: Color::from_hex(0xff6600),
            alpha: 0.8,
            life: 3.0,
            max_life: 3.0,
            rotation: 0.0,
            rotation_speed: 0.0,
            scale: 1.0,
            glow_intensity: 0.6,
        };
        self.particles.push(meteor);
    }

    /// Crear supernova
    fn create_supernova(&mut self) {
        let supernova = SupernovaEffect {
            x: (self.time * 0.3) % self.screen_width as f32,
            y: (self.time * 0.7) % self.screen_height as f32,
            explosion_radius: 0.0,
            max_radius: 100.0,
            shockwave_particles: Vec::new(),
            light_flash_intensity: 1.0,
            animation_phase: 0.0,
            is_active: true,
        };
        self.supernovas.push(supernova);
    }

    /// Renderizar todos los efectos
    pub fn render(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        // Renderizar en orden de profundidad (fondo a frente)
        self.render_stars(fb)?;
        self.render_nebulas(fb)?;
        self.render_auroras(fb)?;
        self.render_black_holes(fb)?;
        self.render_particles(fb)?;
        self.render_supernovas(fb)?;

        Ok(())
    }

    /// Renderizar estrellas
    fn render_stars(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        for particle in &self.particles {
            if particle.particle_type == ParticleType::Star {
                let glow_color = Color {
                    r: (particle.color.r as f32 * particle.glow_intensity * particle.alpha) as u8,
                    g: (particle.color.g as f32 * particle.glow_intensity * particle.alpha) as u8,
                    b: (particle.color.b as f32 * particle.glow_intensity * particle.alpha) as u8,
                    a: 255,
                };

                let x = particle.x as u32;
                let y = particle.y as u32;
                let size = particle.size as u32;

                if x < fb.info.width && y < fb.info.height {
                    fb.draw_rect(x, y, size, size, glow_color);
                }
            }
        }
        Ok(())
    }

    /// Renderizar nebulosas
    fn render_nebulas(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        for nebula in &self.nebulas {
            // Renderizar nebulosa con gradiente
            let steps = 20;
            for i in 0..steps {
                let t = i as f32 / steps as f32;
                let color = self.interpolate_colors(
                    self.interpolate_colors(nebula.color1, nebula.color2, t),
                    nebula.color3,
                    t,
                );

                let alpha_color = Color {
                    r: (color.r as f32 * nebula.opacity) as u8,
                    g: (color.g as f32 * nebula.opacity) as u8,
                    b: (color.b as f32 * nebula.opacity) as u8,
                    a: 255,
                };

                let x = (nebula.x + (i as f32 * nebula.width / steps as f32)) as u32;
                let y = nebula.y as u32;
                let width = (nebula.width / steps as f32) as u32;
                let height = nebula.height as u32;

                if x < fb.info.width && y < fb.info.height {
                    fb.draw_rect(x, y, width, height, alpha_color);
                }
            }
        }
        Ok(())
    }

    /// Renderizar auroras
    fn render_auroras(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        for aurora in &self.auroras {
            let wave_offset = math::sin(aurora.wave_phase * aurora.wave_frequency * 100.0)
                * aurora.wave_amplitude;

            for x in 0..aurora.width as u32 {
                let wave_y = aurora.y + wave_offset + math::sin(x as f32 * 0.02) * 10.0;
                let color_shift = (aurora.color_shift + x as f32 * 0.01) % 1.0;

                let color = if color_shift < 0.33 {
                    Color::from_hex(0x00ff88)
                } else if color_shift < 0.66 {
                    Color::from_hex(0x0088ff)
                } else {
                    Color::from_hex(0xff0088)
                };

                let alpha_color = Color {
                    r: (color.r as f32 * aurora.intensity) as u8,
                    g: (color.g as f32 * aurora.intensity) as u8,
                    b: (color.b as f32 * aurora.intensity) as u8,
                    a: 255,
                };

                let y = wave_y as u32;
                if x < fb.info.width && y < fb.info.height {
                    fb.draw_rect(x, y, 1, 3, alpha_color);
                }
            }
        }
        Ok(())
    }

    /// Renderizar agujeros negros
    fn render_black_holes(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        for black_hole in &self.black_holes {
            // Renderizar disco de acreción
            for particle in &self.particles {
                if particle.particle_type == ParticleType::CosmicDust {
                    let x = particle.x as u32;
                    let y = particle.y as u32;
                    let size = particle.size as u32;

                    if x < fb.info.width && y < fb.info.height {
                        fb.draw_rect(x, y, size, size, particle.color);
                    }
                }
            }

            // Renderizar horizonte de eventos
            let event_horizon_color = Color::from_hex(0x000000);
            let x = (black_hole.x - black_hole.event_horizon) as u32;
            let y = (black_hole.y - black_hole.event_horizon) as u32;
            let size = (black_hole.event_horizon * 2.0) as u32;

            if x < fb.info.width && y < fb.info.height {
                fb.draw_rect(x, y, size, size, event_horizon_color);
            }
        }
        Ok(())
    }

    /// Renderizar partículas especiales
    fn render_particles(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        for particle in &self.particles {
            match particle.particle_type {
                ParticleType::Comet | ParticleType::Meteor | ParticleType::Supernova => {
                    let x = particle.x as u32;
                    let y = particle.y as u32;
                    let size = particle.size as u32;

                    if x < fb.info.width && y < fb.info.height {
                        let alpha_color = Color {
                            r: (particle.color.r as f32 * particle.alpha) as u8,
                            g: (particle.color.g as f32 * particle.alpha) as u8,
                            b: (particle.color.b as f32 * particle.alpha) as u8,
                            a: 255,
                        };

                        fb.draw_rect(x, y, size, size, alpha_color);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Renderizar supernovas
    fn render_supernovas(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        for supernova in &self.supernovas {
            if supernova.is_active && supernova.light_flash_intensity > 0.0 {
                // Flash de luz de la supernova
                let flash_color = Color {
                    r: (255.0 * supernova.light_flash_intensity) as u8,
                    g: (255.0 * supernova.light_flash_intensity) as u8,
                    b: (200.0 * supernova.light_flash_intensity) as u8,
                    a: 255,
                };

                let x = (supernova.x - supernova.explosion_radius) as u32;
                let y = (supernova.y - supernova.explosion_radius) as u32;
                let size = (supernova.explosion_radius * 2.0) as u32;

                if x < fb.info.width && y < fb.info.height {
                    fb.draw_rect(x, y, size, size, flash_color);
                }
            }
        }
        Ok(())
    }

    /// Interpolar entre dos colores
    fn interpolate_colors(&self, color1: Color, color2: Color, t: f32) -> Color {
        Color {
            r: ((color1.r as f32 * (1.0 - t)) + (color2.r as f32 * t)) as u8,
            g: ((color1.g as f32 * (1.0 - t)) + (color2.g as f32 * t)) as u8,
            b: ((color1.b as f32 * (1.0 - t)) + (color2.b as f32 * t)) as u8,
            a: 255,
        }
    }

    /// Obtener color de estrella
    fn get_star_color(&self) -> Color {
        let colors = [
            Color::from_hex(0xffffff),
            Color::from_hex(0xffdddd),
            Color::from_hex(0xddddff),
            Color::from_hex(0xffffdd),
            Color::from_hex(0xffaaff),
        ];
        colors[self.particle_counter as usize % colors.len()]
    }

    /// Obtener colores de nebulosa
    fn get_nebula_color_1(&self, index: usize) -> Color {
        let colors = [
            Color::from_hex(0x440088),
            Color::from_hex(0x008844),
            Color::from_hex(0x884400),
        ];
        colors[index % colors.len()]
    }

    fn get_nebula_color_2(&self, index: usize) -> Color {
        let colors = [
            Color::from_hex(0x8800ff),
            Color::from_hex(0x00ff88),
            Color::from_hex(0xff8800),
        ];
        colors[index % colors.len()]
    }

    fn get_nebula_color_3(&self, index: usize) -> Color {
        let colors = [
            Color::from_hex(0xff00ff),
            Color::from_hex(0x00ffff),
            Color::from_hex(0xffff00),
        ];
        colors[index % colors.len()]
    }

    /// Obtener configuración
    pub fn get_config(&self) -> &VisualEffectsConfig {
        &self.config
    }

    /// Actualizar configuración
    pub fn update_config(&mut self, config: VisualEffectsConfig) {
        self.config = config;
    }

    /// Obtener estadísticas
    pub fn get_stats(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.particles.len(),
            self.nebulas.len(),
            self.black_holes.len(),
            self.auroras.len(),
            self.supernovas.len(),
        )
    }
}
