//! Sistema de Animaciones Fluidas
//! 
//! Este módulo implementa un sistema completo de animaciones:
//! - Animaciones de interpolación suave
//! - Efectos de transición
//! - Animaciones de partículas
//! - Sistema de tweening avanzado

use alloc::vec::Vec;
use alloc::string::{String, ToString};
use alloc::collections::BTreeMap;
use alloc::format;
use core::fmt;

/// Animación
#[derive(Debug, Clone)]
pub struct Animation {
    /// ID único de la animación
    pub id: String,
    /// Tipo de animación
    pub animation_type: AnimationType,
    /// Estado de la animación
    pub state: AnimationState,
    /// Configuración de la animación
    pub config: AnimationConfig,
    /// Progreso actual (0.0 a 1.0)
    pub progress: f32,
    /// Tiempo transcurrido
    pub elapsed_time: u32,
}

/// Tipo de animación
#[derive(Debug, Clone, PartialEq)]
pub enum AnimationType {
    /// Animación de posición
    Position { start_x: f32, start_y: f32, end_x: f32, end_y: f32 },
    /// Animación de escala
    Scale { start_scale: f32, end_scale: f32 },
    /// Animación de rotación
    Rotation { start_angle: f32, end_angle: f32 },
    /// Animación de opacidad
    Opacity { start_alpha: f32, end_alpha: f32 },
    /// Animación de color
    Color { start_color: ColorRGBA, end_color: ColorRGBA },
    /// Animación de tamaño
    Size { start_width: f32, start_height: f32, end_width: f32, end_height: f32 },
    /// Animación de partículas
    Particles { particle_count: u32, particle_type: ParticleType },
}

/// Estado de la animación
#[derive(Debug, Clone, PartialEq)]
pub enum AnimationState {
    /// Preparada para ejecutar
    Ready,
    /// Ejecutándose
    Running,
    /// Pausada
    Paused,
    /// Completada
    Completed,
    /// Cancelada
    Cancelled,
}

/// Configuración de la animación
#[derive(Debug, Clone)]
pub struct AnimationConfig {
    /// Duración en milisegundos
    pub duration: u32,
    /// Tipo de easing
    pub easing: EasingType,
    /// Repetir animación
    pub repeat: RepeatMode,
    /// Delay antes de empezar
    pub delay: u32,
    /// Habilitar reversa
    pub reverse: bool,
    /// Callback al completar
    pub on_complete: Option<String>,
}

/// Modo de repetición
#[derive(Debug, Clone, PartialEq)]
pub enum RepeatMode {
    /// No repetir
    None,
    /// Repetir una vez
    Once,
    /// Repetir infinitamente
    Infinite,
    /// Repetir N veces
    Count(u32),
}

/// Tipo de easing
#[derive(Debug, Clone, PartialEq)]
pub enum EasingType {
    /// Lineal
    Linear,
    /// Ease in
    EaseIn,
    /// Ease out
    EaseOut,
    /// Ease in-out
    EaseInOut,
    /// Bounce
    Bounce,
    /// Elastic
    Elastic,
    /// Back
    Back,
    /// Circ
    Circ,
}

/// Tipo de partícula
#[derive(Debug, Clone, PartialEq)]
pub enum ParticleType {
    /// Partícula simple
    Simple,
    /// Partícula con cola
    Trail,
    /// Partícula explosiva
    Explosion,
    /// Partícula de fuego
    Fire,
    /// Partícula de agua
    Water,
    /// Partícula de nieve
    Snow,
}

/// Color RGBA
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorRGBA {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl ColorRGBA {
    /// Crear nuevo color
    pub fn new(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self { red, green, blue, alpha }
    }
    
    /// Crear desde hexadecimal
    pub fn from_hex(hex: u32) -> Self {
        Self {
            red: ((hex >> 16) & 0xFF) as u8,
            green: ((hex >> 8) & 0xFF) as u8,
            blue: (hex & 0xFF) as u8,
            alpha: 255,
        }
    }
    
    /// Interpolar entre dos colores
    pub fn interpolate(&self, other: &ColorRGBA, t: f32) -> ColorRGBA {
        let t = t.max(0.0).min(1.0);
        
        ColorRGBA {
            red: (self.red as f32 + (other.red as f32 - self.red as f32) * t) as u8,
            green: (self.green as f32 + (other.green as f32 - self.green as f32) * t) as u8,
            blue: (self.blue as f32 + (other.blue as f32 - self.blue as f32) * t) as u8,
            alpha: (self.alpha as f32 + (other.alpha as f32 - self.alpha as f32) * t) as u8,
        }
    }
}

/// Gestor de animaciones
pub struct AnimationManager {
    /// Animaciones activas
    animations: BTreeMap<String, Animation>,
    /// Configuración global
    global_config: AnimationManagerConfig,
    /// Estadísticas
    stats: AnimationStats,
}

/// Configuración del gestor de animaciones
#[derive(Debug, Clone)]
pub struct AnimationManagerConfig {
    /// Habilitar animaciones
    pub enable_animations: bool,
    /// FPS objetivo
    pub target_fps: u32,
    /// Habilitar interpolación suave
    pub enable_smooth_interpolation: bool,
    /// Habilitar animaciones de partículas
    pub enable_particle_animations: bool,
    /// Límite de animaciones simultáneas
    pub max_concurrent_animations: usize,
}

/// Estadísticas de animaciones
#[derive(Debug, Clone)]
pub struct AnimationStats {
    /// Total de animaciones creadas
    pub total_animations: u64,
    /// Animaciones completadas
    pub completed_animations: u64,
    /// Animaciones canceladas
    pub cancelled_animations: u64,
    /// Animaciones activas
    pub active_animations: usize,
    /// FPS actual
    pub current_fps: f32,
    /// Tiempo promedio de animación
    pub average_animation_time: f32,
}

impl AnimationManager {
    /// Crear nuevo gestor de animaciones
    pub fn new() -> Self {
        Self {
            animations: BTreeMap::new(),
            global_config: AnimationManagerConfig {
                enable_animations: true,
                target_fps: 60,
                enable_smooth_interpolation: true,
                enable_particle_animations: true,
                max_concurrent_animations: 100,
            },
            stats: AnimationStats {
                total_animations: 0,
                completed_animations: 0,
                cancelled_animations: 0,
                active_animations: 0,
                current_fps: 0.0,
                average_animation_time: 0.0,
            },
        }
    }
    
    /// Crear animación
    pub fn create_animation(&mut self, id: &str, animation_type: AnimationType, config: AnimationConfig) -> Result<(), String> {
        if !self.global_config.enable_animations {
            return Ok(());
        }
        
        if self.animations.len() >= self.global_config.max_concurrent_animations {
            return Err("Límite de animaciones simultáneas alcanzado".to_string());
        }
        
        let animation = Animation {
            id: id.to_string(),
            animation_type,
            state: AnimationState::Ready,
            config,
            progress: 0.0,
            elapsed_time: 0,
        };
        
        self.animations.insert(id.to_string(), animation);
        self.stats.total_animations += 1;
        self.stats.active_animations = self.animations.len();
        
        Ok(())
    }
    
    /// Iniciar animación
    pub fn start_animation(&mut self, id: &str) -> Result<(), String> {
        if let Some(animation) = self.animations.get_mut(id) {
            if animation.state == AnimationState::Ready {
                animation.state = AnimationState::Running;
                Ok(())
            } else {
                Err("La animación no está lista para ejecutar".to_string())
            }
        } else {
            Err(format!("Animación '{}' no encontrada", id))
        }
    }
    
    /// Pausar animación
    pub fn pause_animation(&mut self, id: &str) -> Result<(), String> {
        if let Some(animation) = self.animations.get_mut(id) {
            if animation.state == AnimationState::Running {
                animation.state = AnimationState::Paused;
                Ok(())
            } else {
                Err("La animación no está ejecutándose".to_string())
            }
        } else {
            Err(format!("Animación '{}' no encontrada", id))
        }
    }
    
    /// Reanudar animación
    pub fn resume_animation(&mut self, id: &str) -> Result<(), String> {
        if let Some(animation) = self.animations.get_mut(id) {
            if animation.state == AnimationState::Paused {
                animation.state = AnimationState::Running;
                Ok(())
            } else {
                Err("La animación no está pausada".to_string())
            }
        } else {
            Err(format!("Animación '{}' no encontrada", id))
        }
    }
    
    /// Cancelar animación
    pub fn cancel_animation(&mut self, id: &str) -> Result<(), String> {
        if let Some(animation) = self.animations.get_mut(id) {
            animation.state = AnimationState::Cancelled;
            self.stats.cancelled_animations += 1;
            self.stats.active_animations = self.animations.len();
            Ok(())
        } else {
            Err(format!("Animación '{}' no encontrada", id))
        }
    }
    
    /// Actualizar animaciones
    pub fn update_animations(&mut self, delta_time: u32) {
        if !self.global_config.enable_animations {
            return;
        }
        
        let mut to_remove = Vec::new();
        
        let animation_ids: Vec<String> = self.animations.keys().cloned().collect();
        for id in animation_ids {
            if let Some(animation) = self.animations.get_mut(&id) {
            if animation.state == AnimationState::Running {
                // Aplicar delay si existe
                if animation.elapsed_time < animation.config.delay {
                    animation.elapsed_time += delta_time;
                    continue;
                }
                
                // Actualizar progreso
                let animation_time = animation.elapsed_time - animation.config.delay;
                animation.progress = (animation_time as f32 / animation.config.duration as f32).min(1.0);
                
                // Aplicar easing
                let eased_progress = match animation.config.easing {
                    EasingType::Linear => animation.progress,
                    EasingType::EaseIn => animation.progress * animation.progress,
                    EasingType::EaseOut => 1.0 - (1.0 - animation.progress) * (1.0 - animation.progress),
                    EasingType::EaseInOut => {
                        if animation.progress < 0.5 {
                            2.0 * animation.progress * animation.progress
                        } else {
                            1.0 - 2.0 * (1.0 - animation.progress) * (1.0 - animation.progress)
                        }
                    },
                    _ => animation.progress, // Simplificado por ahora
                };
                
                // Verificar si la animación ha terminado
                if animation.progress >= 1.0 {
                    match animation.config.repeat {
                        RepeatMode::None => {
                            animation.state = AnimationState::Completed;
                            to_remove.push(id.clone());
                            self.stats.completed_animations += 1;
                        },
                        RepeatMode::Once => {
                            animation.progress = 0.0;
                            animation.elapsed_time = 0;
                            animation.state = AnimationState::Completed;
                            to_remove.push(id.clone());
                            self.stats.completed_animations += 1;
                        },
                        RepeatMode::Infinite => {
                            animation.progress = 0.0;
                            animation.elapsed_time = 0;
                        },
                        RepeatMode::Count(count) => {
                            // Implementar lógica de conteo
                            animation.progress = 0.0;
                            animation.elapsed_time = 0;
                        },
                    }
                } else {
                    animation.elapsed_time += delta_time;
                }
            }
            }
        }
        
        // Remover animaciones completadas
        for id in to_remove {
            self.animations.remove(&id);
        }
        
        self.stats.active_animations = self.animations.len();
    }
    
    /// Aplicar easing
    fn apply_easing(&self, t: f32, easing: &EasingType) -> f32 {
        let t = t.max(0.0).min(1.0);
        
        match easing {
            EasingType::Linear => t,
            EasingType::EaseIn => t * t,
            EasingType::EaseOut => 1.0 - (1.0 - t) * (1.0 - t),
            EasingType::EaseInOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    1.0 - 2.0 * (1.0 - t) * (1.0 - t)
                }
            },
            EasingType::Bounce => {
                if t < 1.0 / 2.75 {
                    7.5625 * t * t
                } else if t < 2.0 / 2.75 {
                    7.5625 * (t - 1.5 / 2.75) * (t - 1.5 / 2.75) + 0.75
                } else if t < 2.5 / 2.75 {
                    7.5625 * (t - 2.25 / 2.75) * (t - 2.25 / 2.75) + 0.9375
                } else {
                    7.5625 * (t - 2.625 / 2.75) * (t - 2.625 / 2.75) + 0.984375
                }
            },
            EasingType::Elastic => {
                if t == 0.0 || t == 1.0 {
                    t
                } else {
                    let c4 = (2.0 * 3.14159) / 3.0;
                    -self.power(2.0, 10.0 * t - 10.0) * self.sin((t * 10.0 - 10.75) * c4)
                }
            },
            EasingType::Back => {
                let c1 = 1.70158;
                let c3 = c1 + 1.0;
                c3 * t * t * t - c1 * t * t
            },
            EasingType::Circ => {
                1.0 - self.sqrt(1.0 - t * t)
            },
        }
    }
    
    /// Implementación simple de potencia para f32
    fn power(&self, base: f32, exponent: f32) -> f32 {
        if exponent == 0.0 {
            return 1.0;
        }
        
        if exponent == 1.0 {
            return base;
        }
        
        if exponent == 2.0 {
            return base * base;
        }
        
        // Implementación simple usando multiplicación repetida
        let mut result = 1.0;
        let abs_exponent = exponent.abs() as i32;
        
        for _ in 0..abs_exponent {
            result *= base;
        }
        
        if exponent < 0.0 {
            1.0 / result
        } else {
            result
        }
    }
    
    /// Implementación simple de seno para f32
    fn sin(&self, x: f32) -> f32 {
        // Normalizar x al rango [-π, π]
        let mut x = x;
        while x > 3.14159 {
            x -= 2.0 * 3.14159;
        }
        while x < -3.14159 {
            x += 2.0 * 3.14159;
        }
        
        // Serie de Taylor para seno
        let x2 = x * x;
        let x3 = x * x2;
        let x5 = x3 * x2;
        let x7 = x5 * x2;
        
        x - x3/6.0 + x5/120.0 - x7/5040.0
    }
    
    /// Implementación simple de raíz cuadrada para f32
    fn sqrt(&self, x: f32) -> f32 {
        if x < 0.0 {
            return 0.0;
        }
        
        if x == 0.0 || x == 1.0 {
            return x;
        }
        
        // Método de Newton-Raphson
        let mut guess = x / 2.0;
        for _ in 0..10 {
            guess = (guess + x / guess) / 2.0;
        }
        
        guess
    }
    
    /// Obtener valor interpolado de animación
    pub fn get_animation_value(&self, id: &str) -> Option<AnimationValue> {
        if let Some(animation) = self.animations.get(id) {
            if animation.state != AnimationState::Running {
                return None;
            }
            
            let eased_progress = self.apply_easing(animation.progress, &animation.config.easing);
            
            match &animation.animation_type {
                AnimationType::Position { start_x, start_y, end_x, end_y } => {
                    Some(AnimationValue::Position {
                        x: start_x + (end_x - start_x) * eased_progress,
                        y: start_y + (end_y - start_y) * eased_progress,
                    })
                },
                AnimationType::Scale { start_scale, end_scale } => {
                    Some(AnimationValue::Scale(start_scale + (end_scale - start_scale) * eased_progress))
                },
                AnimationType::Rotation { start_angle, end_angle } => {
                    Some(AnimationValue::Rotation(start_angle + (end_angle - start_angle) * eased_progress))
                },
                AnimationType::Opacity { start_alpha, end_alpha } => {
                    Some(AnimationValue::Opacity(start_alpha + (end_alpha - start_alpha) * eased_progress))
                },
                AnimationType::Color { start_color, end_color } => {
                    Some(AnimationValue::Color(start_color.interpolate(end_color, eased_progress)))
                },
                AnimationType::Size { start_width, start_height, end_width, end_height } => {
                    Some(AnimationValue::Size {
                        width: start_width + (end_width - start_width) * eased_progress,
                        height: start_height + (end_height - start_height) * eased_progress,
                    })
                },
                AnimationType::Particles { particle_count, particle_type } => {
                    Some(AnimationValue::Particles {
                        count: *particle_count,
                        particle_type: particle_type.clone(),
                        intensity: eased_progress,
                    })
                },
            }
        } else {
            None
        }
    }
    
    /// Obtener estadísticas
    pub fn get_stats(&self) -> &AnimationStats {
        &self.stats
    }
    
    /// Obtener configuración
    pub fn get_config(&self) -> &AnimationManagerConfig {
        &self.global_config
    }
    
    /// Actualizar configuración
    pub fn update_config(&mut self, new_config: AnimationManagerConfig) {
        self.global_config = new_config;
    }
    
    /// Verificar si una animación está activa
    pub fn is_animation_active(&self, id: &str) -> bool {
        if let Some(animation) = self.animations.get(id) {
            animation.state == AnimationState::Running
        } else {
            false
        }
    }
    
    /// Obtener progreso de animación
    pub fn get_animation_progress(&self, id: &str) -> Option<f32> {
        self.animations.get(id).map(|a| a.progress)
    }
}

/// Valor de animación
#[derive(Debug, Clone)]
pub enum AnimationValue {
    /// Posición
    Position { x: f32, y: f32 },
    /// Escala
    Scale(f32),
    /// Rotación
    Rotation(f32),
    /// Opacidad
    Opacity(f32),
    /// Color
    Color(ColorRGBA),
    /// Tamaño
    Size { width: f32, height: f32 },
    /// Partículas
    Particles { count: u32, particle_type: ParticleType, intensity: f32 },
}

impl fmt::Display for AnimationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnimationState::Ready => write!(f, "Lista"),
            AnimationState::Running => write!(f, "Ejecutándose"),
            AnimationState::Paused => write!(f, "Pausada"),
            AnimationState::Completed => write!(f, "Completada"),
            AnimationState::Cancelled => write!(f, "Cancelada"),
        }
    }
}

impl fmt::Display for RepeatMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepeatMode::None => write!(f, "No repetir"),
            RepeatMode::Once => write!(f, "Repetir una vez"),
            RepeatMode::Infinite => write!(f, "Repetir infinitamente"),
            RepeatMode::Count(count) => write!(f, "Repetir {} veces", count),
        }
    }
}
