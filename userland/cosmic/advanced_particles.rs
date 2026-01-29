// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Sistema de efectos de partículas avanzados para COSMIC
pub struct AdvancedParticleSystem {
    /// Sistemas de partículas activos
    particle_systems: VecDeque<ParticleSystem>,
    /// Configuración global
    config: ParticleSystemConfig,
    /// Estadísticas del sistema
    stats: ParticleSystemStats,
    /// Gravedad global
    gravity: f32,
    /// Viento global
    wind: (f32, f32),
}

/// Configuración del sistema de partículas
#[derive(Debug, Clone)]
pub struct ParticleSystemConfig {
    /// Habilitar sistema de partículas
    pub enabled: bool,
    /// Máximo número de sistemas simultáneos
    pub max_systems: usize,
    /// Máximo número de partículas por sistema
    pub max_particles_per_system: usize,
    /// Velocidad de simulación
    pub simulation_speed: f32,
    /// Habilitar física
    pub enable_physics: bool,
    /// Habilitar efectos de viento
    pub enable_wind: bool,
    /// Habilitar colisiones
    pub enable_collisions: bool,
}

impl Default for ParticleSystemConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_systems: 10,
            max_particles_per_system: 500,
            simulation_speed: 1.0,
            enable_physics: true,
            enable_wind: true,
            enable_collisions: false,
        }
    }
}

/// Estadísticas del sistema de partículas
#[derive(Debug, Clone)]
pub struct ParticleSystemStats {
    /// Total de sistemas creados
    pub total_systems: usize,
    /// Sistemas activos actualmente
    pub active_systems: usize,
    /// Total de partículas activas
    pub active_particles: usize,
    /// Sistemas por tipo
    pub systems_by_type: [usize; 8], // StarRain, Explosion, Fire, Smoke, Rain, Snow, Aurora, Custom
    /// FPS de simulación
    pub simulation_fps: f32,
    /// Memoria utilizada
    pub memory_usage: usize,
}

/// Sistema de partículas individual
#[derive(Debug, Clone)]
pub struct ParticleSystem {
    /// ID único del sistema
    pub id: String,
    /// Tipo de sistema
    pub system_type: ParticleSystemType,
    /// Posición del emisor
    pub emitter_position: (f32, f32),
    /// Partículas del sistema
    pub particles: Vec<Particle>,
    /// Configuración específica
    pub config: ParticleSystemSettings,
    /// Estado del sistema
    pub state: ParticleSystemState,
    /// Tiempo de vida del sistema
    pub lifetime: f32,
    /// Tiempo máximo de vida
    pub max_lifetime: f32,
}

/// Tipo de sistema de partículas
#[derive(Debug, Clone, PartialEq)]
pub enum ParticleSystemType {
    /// Lluvia de estrellas
    StarRain,
    /// Explosión
    Explosion,
    /// Fuego
    Fire,
    /// Humo
    Smoke,
    /// Lluvia
    Rain,
    /// Nieve
    Snow,
    /// Aurora boreal
    Aurora,
    /// Sistema personalizado
    Custom,
}

/// Estado del sistema de partículas
#[derive(Debug, Clone, PartialEq)]
pub enum ParticleSystemState {
    /// Emitiendo partículas
    Emitting,
    /// Partículas activas
    Active,
    /// Extinguiéndose
    Dying,
    /// Inactivo
    Inactive,
}

/// Configuración específica del sistema
#[derive(Debug, Clone)]
pub struct ParticleSystemSettings {
    /// Velocidad de emisión
    pub emission_rate: f32,
    /// Velocidad inicial de las partículas
    pub initial_velocity: f32,
    /// Dirección de emisión
    pub emission_direction: f32,
    /// Dispersión angular
    pub angular_spread: f32,
    /// Tiempo de vida de las partículas
    pub particle_lifetime: f32,
    /// Tamaño de las partículas
    pub particle_size: f32,
    /// Color base de las partículas
    pub base_color: Color,
    /// Variación de color
    pub color_variation: f32,
    /// Gravedad local
    pub local_gravity: f32,
    /// Resistencia del aire
    pub air_resistance: f32,
}

/// Partícula individual
#[derive(Debug, Clone)]
pub struct Particle {
    /// Posición actual
    pub position: (f32, f32),
    /// Velocidad actual
    pub velocity: (f32, f32),
    /// Aceleración
    pub acceleration: (f32, f32),
    /// Tiempo de vida restante
    pub remaining_life: f32,
    /// Tiempo de vida total
    pub total_life: f32,
    /// Tamaño actual
    pub size: f32,
    /// Color actual
    pub color: Color,
    /// Opacidad actual
    pub opacity: f32,
    /// Rotación actual
    pub rotation: f32,
    /// Velocidad de rotación
    pub rotation_speed: f32,
    /// Masa de la partícula
    pub mass: f32,
    /// Tipo de partícula
    pub particle_type: ParticleType,
}

/// Tipo de partícula
#[derive(Debug, Clone, PartialEq)]
pub enum ParticleType {
    /// Partícula estándar
    Standard,
    /// Partícula con física
    Physics,
    /// Partícula de fuego
    Fire,
    /// Partícula de humo
    Smoke,
    /// Partícula de agua
    Water,
    /// Partícula de hielo
    Ice,
    /// Partícula de energía
    Energy,
}

impl AdvancedParticleSystem {
    /// Crear nuevo sistema de partículas avanzado
    pub fn new() -> Self {
        Self {
            particle_systems: VecDeque::new(),
            config: ParticleSystemConfig::default(),
            stats: ParticleSystemStats {
                total_systems: 0,
                active_systems: 0,
                active_particles: 0,
                systems_by_type: [0, 0, 0, 0, 0, 0, 0, 0],
                simulation_fps: 0.0,
                memory_usage: 0,
            },
            gravity: 0.5,
            wind: (0.1, 0.0),
        }
    }

    /// Crear sistema con configuración personalizada
    pub fn with_config(config: ParticleSystemConfig) -> Self {
        Self {
            particle_systems: VecDeque::new(),
            config,
            stats: ParticleSystemStats {
                total_systems: 0,
                active_systems: 0,
                active_particles: 0,
                systems_by_type: [0, 0, 0, 0, 0, 0, 0, 0],
                simulation_fps: 0.0,
                memory_usage: 0,
            },
            gravity: 0.5,
            wind: (0.1, 0.0),
        }
    }

    /// Crear un nuevo sistema de partículas
    pub fn create_particle_system(
        &mut self,
        system_type: ParticleSystemType,
        position: (f32, f32),
    ) -> String {
        if !self.config.enabled {
            return String::from("");
        }

        let system_id = alloc::format!("particle_system_{}", self.stats.total_systems);
        let settings = self.create_system_settings(&system_type);

        let system_type_clone = system_type.clone();
        let mut system = ParticleSystem {
            id: system_id.clone(),
            system_type,
            emitter_position: position,
            particles: Vec::new(),
            config: settings,
            state: ParticleSystemState::Emitting,
            lifetime: 0.0,
            max_lifetime: self.get_max_lifetime(&system_type_clone),
        };

        // Crear partículas iniciales
        self.emit_initial_particles(&mut system);

        self.particle_systems.push_front(system);

        // Limitar número de sistemas
        while self.particle_systems.len() > self.config.max_systems {
            self.particle_systems.pop_back();
        }

        self.stats.total_systems += 1;
        self.update_stats();

        system_id
    }

    /// Crear configuración del sistema según su tipo
    fn create_system_settings(&self, system_type: &ParticleSystemType) -> ParticleSystemSettings {
        match system_type {
            ParticleSystemType::StarRain => ParticleSystemSettings {
                emission_rate: 20.0,
                initial_velocity: 2.0,
                emission_direction: -90.0, // Hacia abajo
                angular_spread: 30.0,
                particle_lifetime: 8.0,
                particle_size: 2.0,
                base_color: Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                },
                color_variation: 0.3,
                local_gravity: 0.3,
                air_resistance: 0.02,
            },
            ParticleSystemType::Explosion => ParticleSystemSettings {
                emission_rate: 100.0,
                initial_velocity: 8.0,
                emission_direction: 0.0,
                angular_spread: 360.0,
                particle_lifetime: 2.0,
                particle_size: 3.0,
                base_color: Color {
                    r: 255,
                    g: 100,
                    b: 0,
                    a: 255,
                },
                color_variation: 0.5,
                local_gravity: 0.1,
                air_resistance: 0.05,
            },
            ParticleSystemType::Fire => ParticleSystemSettings {
                emission_rate: 30.0,
                initial_velocity: 1.5,
                emission_direction: -90.0,
                angular_spread: 45.0,
                particle_lifetime: 3.0,
                particle_size: 2.5,
                base_color: Color {
                    r: 255,
                    g: 50,
                    b: 0,
                    a: 255,
                },
                color_variation: 0.4,
                local_gravity: -0.2, // Fuego sube
                air_resistance: 0.03,
            },
            ParticleSystemType::Smoke => ParticleSystemSettings {
                emission_rate: 15.0,
                initial_velocity: 1.0,
                emission_direction: -90.0,
                angular_spread: 60.0,
                particle_lifetime: 5.0,
                particle_size: 4.0,
                base_color: Color {
                    r: 100,
                    g: 100,
                    b: 100,
                    a: 150,
                },
                color_variation: 0.2,
                local_gravity: -0.1,
                air_resistance: 0.01,
            },
            ParticleSystemType::Rain => ParticleSystemSettings {
                emission_rate: 50.0,
                initial_velocity: 4.0,
                emission_direction: -90.0,
                angular_spread: 10.0,
                particle_lifetime: 3.0,
                particle_size: 1.0,
                base_color: Color {
                    r: 100,
                    g: 150,
                    b: 255,
                    a: 200,
                },
                color_variation: 0.1,
                local_gravity: 0.8,
                air_resistance: 0.01,
            },
            ParticleSystemType::Snow => ParticleSystemSettings {
                emission_rate: 25.0,
                initial_velocity: 0.5,
                emission_direction: -90.0,
                angular_spread: 20.0,
                particle_lifetime: 10.0,
                particle_size: 2.0,
                base_color: Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 200,
                },
                color_variation: 0.1,
                local_gravity: 0.1,
                air_resistance: 0.02,
            },
            ParticleSystemType::Aurora => ParticleSystemSettings {
                emission_rate: 10.0,
                initial_velocity: 0.8,
                emission_direction: 0.0,
                angular_spread: 180.0,
                particle_lifetime: 6.0,
                particle_size: 3.0,
                base_color: Color {
                    r: 0,
                    g: 255,
                    b: 100,
                    a: 150,
                },
                color_variation: 0.6,
                local_gravity: 0.0,
                air_resistance: 0.005,
            },
            ParticleSystemType::Custom => ParticleSystemSettings {
                emission_rate: 20.0,
                initial_velocity: 2.0,
                emission_direction: 0.0,
                angular_spread: 360.0,
                particle_lifetime: 4.0,
                particle_size: 2.0,
                base_color: Color {
                    r: 150,
                    g: 150,
                    b: 255,
                    a: 200,
                },
                color_variation: 0.3,
                local_gravity: 0.2,
                air_resistance: 0.02,
            },
        }
    }

    /// Obtener tiempo máximo de vida según el tipo
    fn get_max_lifetime(&self, system_type: &ParticleSystemType) -> f32 {
        match system_type {
            ParticleSystemType::StarRain => 15.0,
            ParticleSystemType::Explosion => 3.0,
            ParticleSystemType::Fire => 8.0,
            ParticleSystemType::Smoke => 10.0,
            ParticleSystemType::Rain => 5.0,
            ParticleSystemType::Snow => 20.0,
            ParticleSystemType::Aurora => 12.0,
            ParticleSystemType::Custom => 6.0,
        }
    }

    /// Emitir partículas iniciales
    fn emit_initial_particles(&mut self, system: &mut ParticleSystem) {
        let initial_count = (system.config.emission_rate * 0.5) as usize;
        for _ in 0..initial_count {
            let particle = self.create_particle(system);
            system.particles.push(particle);
        }
    }

    /// Crear una nueva partícula (método estático)
    fn create_particle_static(system: &ParticleSystem) -> Particle {
        let angle = system.config.emission_direction
            + (system.config.angular_spread * (Self::random_float_static() - 0.5));
        let velocity = system.config.initial_velocity * (0.8 + Self::random_float_static() * 0.4);

        let vx = velocity * Self::cos_approx_static(angle.to_radians());
        let vy = velocity * Self::sin_approx_static(angle.to_radians());

        let color_variation = system.config.color_variation * (Self::random_float_static() - 0.5);
        let color = Self::vary_color_static(&system.config.base_color, color_variation);

        Particle {
            position: system.emitter_position,
            velocity: (vx, vy),
            acceleration: (0.0, 0.0),
            remaining_life: system.config.particle_lifetime,
            total_life: system.config.particle_lifetime,
            size: system.config.particle_size * (0.8 + Self::random_float_static() * 0.4),
            color,
            opacity: 1.0,
            rotation: 0.0,
            rotation_speed: (Self::random_float_static() - 0.5) * 10.0,
            mass: 1.0,
            particle_type: Self::get_particle_type_static(&system.system_type),
        }
    }

    /// Crear una nueva partícula
    fn create_particle(&self, system: &ParticleSystem) -> Particle {
        let angle = system.config.emission_direction
            + (system.config.angular_spread * (self.random_float() - 0.5));
        let velocity = system.config.initial_velocity * (0.8 + self.random_float() * 0.4);

        let vx = velocity * self.cos_approx(angle.to_radians());
        let vy = velocity * self.sin_approx(angle.to_radians());

        let color_variation = system.config.color_variation * (self.random_float() - 0.5);
        let color = self.vary_color(&system.config.base_color, color_variation);

        Particle {
            position: system.emitter_position,
            velocity: (vx, vy),
            acceleration: (0.0, 0.0),
            remaining_life: system.config.particle_lifetime,
            total_life: system.config.particle_lifetime,
            size: system.config.particle_size * (0.8 + self.random_float() * 0.4),
            color,
            opacity: 1.0,
            rotation: 0.0,
            rotation_speed: (self.random_float() - 0.5) * 10.0,
            mass: 1.0,
            particle_type: self.get_particle_type(&system.system_type),
        }
    }

    /// Obtener tipo de partícula según el sistema
    fn get_particle_type(&self, system_type: &ParticleSystemType) -> ParticleType {
        match system_type {
            ParticleSystemType::Fire => ParticleType::Fire,
            ParticleSystemType::Smoke => ParticleType::Smoke,
            ParticleSystemType::Rain => ParticleType::Water,
            ParticleSystemType::Snow => ParticleType::Ice,
            ParticleSystemType::Aurora => ParticleType::Energy,
            _ => ParticleType::Standard,
        }
    }

    /// Actualizar todos los sistemas de partículas
    pub fn update(&mut self, delta_time: f32) {
        if !self.config.enabled {
            return;
        }

        // Crear una copia de los sistemas para evitar problemas de borrowing
        let mut systems_to_update: Vec<_> = self.particle_systems.iter_mut().collect();

        for system in &mut systems_to_update {
            system.lifetime += delta_time;

            // Emitir nuevas partículas
            if system.state == ParticleSystemState::Emitting {
                Self::emit_particles_static(system, delta_time, &self.config);
            }

            // Actualizar partículas existentes
            Self::update_particles_static(
                system,
                delta_time,
                &self.config,
                self.gravity,
                self.wind,
            );

            // Limpiar partículas muertas
            system.particles.retain(|p| p.remaining_life > 0.0);

            // Actualizar estado del sistema
            Self::update_system_state_static(system);
        }

        // Eliminar sistemas inactivos
        self.particle_systems
            .retain(|s| s.state != ParticleSystemState::Inactive);
        self.update_stats();
    }

    /// Emitir nuevas partículas (método estático)
    fn emit_particles_static(
        system: &mut ParticleSystem,
        delta_time: f32,
        config: &ParticleSystemConfig,
    ) {
        if system.particles.len() >= config.max_particles_per_system {
            return;
        }

        let particles_to_emit = (system.config.emission_rate * delta_time) as usize;
        for _ in 0..particles_to_emit {
            let particle = Self::create_particle_static(system);
            system.particles.push(particle);
        }
    }

    /// Actualizar partículas del sistema (método estático)
    fn update_particles_static(
        system: &mut ParticleSystem,
        delta_time: f32,
        config: &ParticleSystemConfig,
        gravity: f32,
        wind: (f32, f32),
    ) {
        // Crear una copia de la configuración para evitar borrowing
        let local_gravity = system.config.local_gravity;
        let air_resistance = system.config.air_resistance;

        for particle in &mut system.particles {
            // Aplicar física simplificada
            if config.enable_physics {
                // Gravedad
                let total_gravity = gravity + local_gravity;
                particle.velocity.1 += total_gravity * delta_time;

                // Resistencia del aire
                particle.velocity.0 *= 1.0 - air_resistance * delta_time;
                particle.velocity.1 *= 1.0 - air_resistance * delta_time;

                // Efectos específicos por tipo
                match particle.particle_type {
                    ParticleType::Fire => {
                        // Fuego sube y se mueve aleatoriamente
                        particle.velocity.1 -= 0.5 * delta_time;
                        particle.velocity.0 += 0.5 * delta_time; // Simplificado
                    }
                    ParticleType::Smoke => {
                        // Humo sube y se dispersa
                        particle.velocity.1 -= 0.3 * delta_time;
                        particle.velocity.0 += 0.2 * delta_time; // Simplificado
                    }
                    ParticleType::Water => {
                        // Agua cae con gravedad
                        particle.velocity.1 += 1.0 * delta_time;
                    }
                    ParticleType::Ice => {
                        // Hielo cae lentamente
                        particle.velocity.1 += 0.2 * delta_time;
                        particle.velocity.0 += 0.1 * delta_time; // Simplificado
                    }
                    ParticleType::Energy => {
                        // Energía se mueve en ondas
                        let wave = Self::sin_approx_static(particle.rotation * 0.1);
                        particle.velocity.0 += wave * 0.5 * delta_time;
                        particle.velocity.1 +=
                            Self::cos_approx_static(particle.rotation * 0.1) * 0.5 * delta_time;
                    }
                    _ => {}
                }
            }

            // Aplicar viento
            if config.enable_wind {
                particle.velocity.0 += wind.0 * delta_time;
                particle.velocity.1 += wind.1 * delta_time;
            }

            // Actualizar posición
            particle.position.0 += particle.velocity.0 * delta_time;
            particle.position.1 += particle.velocity.1 * delta_time;

            // Actualizar rotación
            particle.rotation += particle.rotation_speed * delta_time;

            // Actualizar tiempo de vida
            particle.remaining_life -= delta_time;

            // Actualizar opacidad
            let life_ratio = particle.remaining_life / particle.total_life;
            particle.opacity = life_ratio.max(0.0);

            // Actualizar tamaño (algunas partículas se encogen)
            match particle.particle_type {
                ParticleType::Fire => particle.size *= 1.0 + delta_time * 0.5,
                ParticleType::Smoke => particle.size *= 1.0 + delta_time * 0.3,
                _ => particle.size *= 1.0 - delta_time * 0.1,
            }
        }
    }

    /// Aplicar física a una partícula
    fn apply_physics(&self, particle: &mut Particle, system: &ParticleSystem, delta_time: f32) {
        // Gravedad
        let gravity = self.gravity + system.config.local_gravity;
        particle.velocity.1 += gravity * delta_time;

        // Resistencia del aire
        let resistance = system.config.air_resistance;
        particle.velocity.0 *= 1.0 - resistance * delta_time;
        particle.velocity.1 *= 1.0 - resistance * delta_time;

        // Efectos específicos por tipo
        match particle.particle_type {
            ParticleType::Fire => {
                // Fuego sube y se mueve aleatoriamente
                particle.velocity.1 -= 0.5 * delta_time;
                particle.velocity.0 += (self.random_float() - 0.5) * 2.0 * delta_time;
            }
            ParticleType::Smoke => {
                // Humo sube y se dispersa
                particle.velocity.1 -= 0.3 * delta_time;
                particle.velocity.0 += (self.random_float() - 0.5) * 1.0 * delta_time;
            }
            ParticleType::Water => {
                // Agua cae con gravedad
                particle.velocity.1 += 1.0 * delta_time;
            }
            ParticleType::Ice => {
                // Hielo cae lentamente
                particle.velocity.1 += 0.2 * delta_time;
                particle.velocity.0 += (self.random_float() - 0.5) * 0.5 * delta_time;
            }
            ParticleType::Energy => {
                // Energía se mueve en ondas
                let wave = self.sin_approx(particle.rotation * 0.1);
                particle.velocity.0 += wave * 0.5 * delta_time;
                particle.velocity.1 += self.cos_approx(particle.rotation * 0.1) * 0.5 * delta_time;
            }
            _ => {}
        }
    }

    /// Actualizar estado del sistema
    fn update_system_state(&mut self, system: &mut ParticleSystem) {
        match system.state {
            ParticleSystemState::Emitting => {
                if system.lifetime > system.max_lifetime * 0.7 {
                    system.state = ParticleSystemState::Active;
                }
            }
            ParticleSystemState::Active => {
                if system.particles.is_empty() || system.lifetime > system.max_lifetime {
                    system.state = ParticleSystemState::Dying;
                }
            }
            ParticleSystemState::Dying => {
                if system.particles.is_empty() {
                    system.state = ParticleSystemState::Inactive;
                }
            }
            ParticleSystemState::Inactive => {}
        }
    }

    /// Renderizar todos los sistemas de partículas
    pub fn render(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        if !self.config.enabled {
            return Ok(());
        }

        // Renderizar sistemas en orden de profundidad
        let systems_copy: Vec<_> = self.particle_systems.iter().collect();
        for system in systems_copy {
            Self::render_particle_system(fb, system)?;
        }

        Ok(())
    }

    /// Renderizar un sistema de partículas
    fn render_particle_system(
        fb: &mut FramebufferDriver,
        system: &ParticleSystem,
    ) -> Result<(), String> {
        for particle in &system.particles {
            if particle.opacity > 0.01 {
                Self::render_particle(fb, particle)?;
            }
        }
        Ok(())
    }

    /// Renderizar una partícula individual
    fn render_particle(fb: &mut FramebufferDriver, particle: &Particle) -> Result<(), String> {
        let x = particle.position.0 as u32;
        let y = particle.position.1 as u32;
        let size = particle.size as u32;

        if x < fb.info.width && y < fb.info.height && size > 0 {
            let color = Color {
                r: particle.color.r,
                g: particle.color.g,
                b: particle.color.b,
                a: (particle.color.a as f32 * particle.opacity) as u8,
            };

            // Renderizar según el tipo de partícula
            match particle.particle_type {
                ParticleType::Fire => {
                    // Fuego con forma de llama
                    for i in 0..size {
                        let intensity = 1.0 - (i as f32 / size as f32);
                        let fire_color = Color {
                            r: (color.r as f32 * intensity) as u8,
                            g: (color.g as f32 * intensity * 0.5) as u8,
                            b: 0,
                            a: color.a,
                        };
                        fb.draw_rect(x, y + i, size - i, 1, fire_color);
                    }
                }
                ParticleType::Smoke => {
                    // Humo con forma circular
                    let radius = size / 2;
                    for dy in 0..size {
                        for dx in 0..size {
                            let distance = ((dx as i32 - radius as i32).abs()
                                + (dy as i32 - radius as i32).abs())
                                as u32;
                            if distance <= radius {
                                let intensity = 1.0 - (distance as f32 / radius as f32);
                                let smoke_color = Color {
                                    r: (color.r as f32 * intensity) as u8,
                                    g: (color.g as f32 * intensity) as u8,
                                    b: (color.b as f32 * intensity) as u8,
                                    a: (color.a as f32 * intensity) as u8,
                                };
                                fb.draw_rect(x + dx, y + dy, 1, 1, smoke_color);
                            }
                        }
                    }
                }
                ParticleType::Water => {
                    // Agua con forma de línea
                    fb.draw_rect(x, y, 1, size, color);
                }
                ParticleType::Ice => {
                    // Hielo con forma de estrella
                    let center = size / 2;
                    for i in 0..size {
                        let intensity = 0.8 + 0.2 * ((i as f32 / size as f32) * 2.0 - 1.0).abs();
                        let ice_color = Color {
                            r: (color.r as f32 * intensity) as u8,
                            g: (color.g as f32 * intensity) as u8,
                            b: (color.b as f32 * intensity) as u8,
                            a: color.a,
                        };
                        fb.draw_rect(x + i, y + center, 1, 1, ice_color);
                        fb.draw_rect(x + center, y + i, 1, 1, ice_color);
                    }
                }
                ParticleType::Energy => {
                    // Energía con forma de pulso
                    let pulse = (particle.rotation * 0.5).abs() % 2.0;
                    let intensity = if pulse < 1.0 { pulse } else { 2.0 - pulse };
                    let energy_color = Color {
                        r: (color.r as f32 * intensity) as u8,
                        g: (color.g as f32 * intensity) as u8,
                        b: (color.b as f32 * intensity) as u8,
                        a: color.a,
                    };
                    fb.draw_rect(x, y, size, size, energy_color);
                }
                _ => {
                    // Partícula estándar
                    fb.draw_rect(x, y, size, size, color);
                }
            }
        }

        Ok(())
    }

    /// Variar color con una variación aleatoria
    fn vary_color(&self, base_color: &Color, variation: f32) -> Color {
        Color {
            r: ((base_color.r as f32 + variation * 255.0)
                .max(0.0)
                .min(255.0)) as u8,
            g: ((base_color.g as f32 + variation * 255.0)
                .max(0.0)
                .min(255.0)) as u8,
            b: ((base_color.b as f32 + variation * 255.0)
                .max(0.0)
                .min(255.0)) as u8,
            a: base_color.a,
        }
    }

    /// Generar número aleatorio simple
    fn random_float(&self) -> f32 {
        // Generador aleatorio simple basado en el tiempo
        let time = (self.stats.total_systems as f32 * 0.1) % 1.0;
        (time * 7.0) % 1.0
    }

    /// Aproximación de seno para no_std
    fn sin_approx(&self, x: f32) -> f32 {
        let x = x % (2.0 * 3.14159);
        if x < 0.0 {
            -self.sin_approx(-x)
        } else if x <= 3.14159 / 2.0 {
            x - (x * x * x) / 6.0 + (x * x * x * x * x) / 120.0
        } else if x <= 3.14159 {
            self.sin_approx(3.14159 - x)
        } else {
            -self.sin_approx(x - 3.14159)
        }
    }

    /// Aproximación de coseno para no_std
    fn cos_approx(&self, x: f32) -> f32 {
        self.sin_approx(x + 3.14159 / 2.0)
    }

    /// Actualizar estadísticas
    fn update_stats(&mut self) {
        self.stats.active_systems = self
            .particle_systems
            .iter()
            .filter(|s| s.state != ParticleSystemState::Inactive)
            .count();
        self.stats.active_particles = self
            .particle_systems
            .iter()
            .map(|s| s.particles.len())
            .sum();
        self.stats.memory_usage =
            self.particle_systems.len() * core::mem::size_of::<ParticleSystem>();

        // Contar sistemas por tipo
        self.stats.systems_by_type = [0, 0, 0, 0, 0, 0, 0, 0];
        for system in &self.particle_systems {
            match system.system_type {
                ParticleSystemType::StarRain => self.stats.systems_by_type[0] += 1,
                ParticleSystemType::Explosion => self.stats.systems_by_type[1] += 1,
                ParticleSystemType::Fire => self.stats.systems_by_type[2] += 1,
                ParticleSystemType::Smoke => self.stats.systems_by_type[3] += 1,
                ParticleSystemType::Rain => self.stats.systems_by_type[4] += 1,
                ParticleSystemType::Snow => self.stats.systems_by_type[5] += 1,
                ParticleSystemType::Aurora => self.stats.systems_by_type[6] += 1,
                ParticleSystemType::Custom => self.stats.systems_by_type[7] += 1,
            }
        }
    }

    /// Obtener estadísticas
    pub fn get_stats(&self) -> &ParticleSystemStats {
        &self.stats
    }

    /// Configurar el sistema
    pub fn configure(&mut self, config: ParticleSystemConfig) {
        self.config = config;
    }

    /// Obtener configuración
    pub fn get_config(&self) -> &ParticleSystemConfig {
        &self.config
    }

    /// Limpiar todos los sistemas
    pub fn clear_all_systems(&mut self) {
        self.particle_systems.clear();
        self.update_stats();
    }

    /// Habilitar/deshabilitar sistema de partículas
    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
    }

    /// Crear sistemas de ejemplo
    pub fn create_sample_systems(&mut self) -> Vec<String> {
        let mut system_ids = Vec::new();

        // Lluvia de estrellas en la parte superior
        let star_rain_id = self.create_particle_system(ParticleSystemType::StarRain, (960.0, 50.0));
        system_ids.push(star_rain_id);

        // Fuego en el centro
        let fire_id = self.create_particle_system(ParticleSystemType::Fire, (960.0, 800.0));
        system_ids.push(fire_id);

        // Aurora en los lados
        let aurora_id = self.create_particle_system(ParticleSystemType::Aurora, (200.0, 400.0));
        system_ids.push(aurora_id);

        system_ids
    }

    // Métodos estáticos auxiliares
    fn apply_physics_static(
        particle: &mut Particle,
        system: &ParticleSystem,
        delta_time: f32,
        gravity: f32,
    ) {
        // Gravedad
        let gravity = gravity + system.config.local_gravity;
        particle.velocity.1 += gravity * delta_time;

        // Resistencia del aire
        let resistance = system.config.air_resistance;
        particle.velocity.0 *= 1.0 - resistance * delta_time;
        particle.velocity.1 *= 1.0 - resistance * delta_time;

        // Efectos específicos por tipo
        match particle.particle_type {
            ParticleType::Fire => {
                // Fuego sube y se mueve aleatoriamente
                particle.velocity.1 -= 0.5 * delta_time;
                particle.velocity.0 += (Self::random_float_static() - 0.5) * 2.0 * delta_time;
            }
            ParticleType::Smoke => {
                // Humo sube y se dispersa
                particle.velocity.1 -= 0.3 * delta_time;
                particle.velocity.0 += (Self::random_float_static() - 0.5) * 1.0 * delta_time;
            }
            ParticleType::Water => {
                // Agua cae con gravedad
                particle.velocity.1 += 1.0 * delta_time;
            }
            ParticleType::Ice => {
                // Hielo cae lentamente
                particle.velocity.1 += 0.2 * delta_time;
                particle.velocity.0 += (Self::random_float_static() - 0.5) * 0.5 * delta_time;
            }
            ParticleType::Energy => {
                // Energía se mueve en ondas
                let wave = Self::sin_approx_static(particle.rotation * 0.1);
                particle.velocity.0 += wave * 0.5 * delta_time;
                particle.velocity.1 +=
                    Self::cos_approx_static(particle.rotation * 0.1) * 0.5 * delta_time;
            }
            _ => {}
        }
    }

    fn update_system_state_static(system: &mut ParticleSystem) {
        match system.state {
            ParticleSystemState::Emitting => {
                if system.lifetime > system.max_lifetime * 0.7 {
                    system.state = ParticleSystemState::Active;
                }
            }
            ParticleSystemState::Active => {
                if system.particles.is_empty() || system.lifetime > system.max_lifetime {
                    system.state = ParticleSystemState::Dying;
                }
            }
            ParticleSystemState::Dying => {
                if system.particles.is_empty() {
                    system.state = ParticleSystemState::Inactive;
                }
            }
            ParticleSystemState::Inactive => {}
        }
    }

    fn get_particle_type_static(system_type: &ParticleSystemType) -> ParticleType {
        match system_type {
            ParticleSystemType::Fire => ParticleType::Fire,
            ParticleSystemType::Smoke => ParticleType::Smoke,
            ParticleSystemType::Rain => ParticleType::Water,
            ParticleSystemType::Snow => ParticleType::Ice,
            ParticleSystemType::Aurora => ParticleType::Energy,
            _ => ParticleType::Standard,
        }
    }

    fn random_float_static() -> f32 {
        // Generador aleatorio simple
        0.5 // Simplificado para evitar problemas de borrowing
    }

    fn sin_approx_static(x: f32) -> f32 {
        let x = x % (2.0 * 3.14159);
        if x < 0.0 {
            -Self::sin_approx_static(-x)
        } else if x <= 3.14159 / 2.0 {
            x - (x * x * x) / 6.0 + (x * x * x * x * x) / 120.0
        } else if x <= 3.14159 {
            Self::sin_approx_static(3.14159 - x)
        } else {
            -Self::sin_approx_static(x - 3.14159)
        }
    }

    fn cos_approx_static(x: f32) -> f32 {
        Self::sin_approx_static(x + 3.14159 / 2.0)
    }

    fn vary_color_static(base_color: &Color, variation: f32) -> Color {
        Color {
            r: ((base_color.r as f32 + variation * 255.0)
                .max(0.0)
                .min(255.0)) as u8,
            g: ((base_color.g as f32 + variation * 255.0)
                .max(0.0)
                .min(255.0)) as u8,
            b: ((base_color.b as f32 + variation * 255.0)
                .max(0.0)
                .min(255.0)) as u8,
            a: base_color.a,
        }
    }
}
