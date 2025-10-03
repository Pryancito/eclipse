use crate::drivers::framebuffer::Color;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

/// Tipos de funciones de easing
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EasingFunction {
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    EaseInCubic,
    EaseOutCubic,
    EaseInOutCubic,
    EaseInQuart,
    EaseOutQuart,
    EaseInOutQuart,
    EaseInQuint,
    EaseOutQuint,
    EaseInOutQuint,
    EaseInSine,
    EaseOutSine,
    EaseInOutSine,
    EaseInExpo,
    EaseOutExpo,
    EaseInOutExpo,
    EaseInCirc,
    EaseOutCirc,
    EaseInOutCirc,
    EaseInBack,
    EaseOutBack,
    EaseInOutBack,
    EaseInElastic,
    EaseOutElastic,
    EaseInOutElastic,
    EaseInBounce,
    EaseOutBounce,
    EaseInOutBounce,
}

/// Tipos de animación
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnimationType {
    Color,
    Position,
    Scale,
    Opacity,
    BorderRadius,
    ShadowBlur,
    Rotation,
    Skew,
    Translation,
    Fade,
    Slide,
    Zoom,
    Flip,
    Shake,
    Pulse,
    Glow,
    Morph,
}

/// Configuración de una animación
#[derive(Debug, Clone)]
pub struct AnimationConfig {
    pub duration: Duration,
    pub easing: EasingFunction,
    pub animation_type: AnimationType,
    pub repeat: bool,
    pub reverse: bool,
}

/// Propiedades animables de un widget
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnimatableProperties {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub color: Color,
    pub scale_x: f32,
    pub scale_y: f32,
    pub opacity: f32,
    pub border_radius: f32,
    pub shadow_blur: f32,
    pub rotation: f32,
    pub skew_x: f32,
    pub skew_y: f32,
    pub translation_x: f32,
    pub translation_y: f32,
    pub glow_intensity: f32,
    pub glow_color: Color,
    pub morph_factor: f32,
}

impl Default for AnimatableProperties {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
            color: Color::from_rgba(0, 0, 0, 0), // Transparente por defecto
            scale_x: 1.0,
            scale_y: 1.0,
            opacity: 1.0,
            border_radius: 0.0,
            shadow_blur: 0.0,
            rotation: 0.0,
            skew_x: 0.0,
            skew_y: 0.0,
            translation_x: 0.0,
            translation_y: 0.0,
            glow_intensity: 0.0,
            glow_color: Color::from_rgba(0, 0, 0, 0),
            morph_factor: 0.0,
        }
    }
}

/// Estado de una animación
#[derive(Debug, Clone)]
pub struct AnimationState {
    pub id: u32,
    pub start_time: u64,
    pub end_time: u64,
    pub config: AnimationConfig,
    pub current_progress: f32, // 0.0 a 1.0
    pub is_active: bool,
    pub start_properties: AnimatableProperties,
    pub end_properties: AnimatableProperties,
    pub current_properties: AnimatableProperties,
}

/// Gestor de animaciones
#[derive(Debug)]
pub struct AnimationManager {
    animations: BTreeMap<u32, AnimationState>,
    next_id: u32,
    current_time: u64,
}

impl AnimationManager {
    pub fn new() -> Self {
        Self {
            animations: BTreeMap::new(),
            next_id: 1,
            current_time: 0,
        }
    }

    pub fn update_time(&mut self, new_time: u64) {
        self.current_time = new_time;
    }

    pub fn create_animation(
        &mut self,
        start_props: AnimatableProperties,
        end_props: AnimatableProperties,
        config: AnimationConfig,
    ) -> u32 {
        let id = self.next_id;
        self.next_id += 1;

        let start_time = self.current_time;
        let end_time = start_time + config.duration.as_millis() as u64;

        let animation_state = AnimationState {
            id,
            start_time,
            end_time,
            config,
            current_progress: 0.0,
            is_active: false,
            start_properties: start_props,
            end_properties: end_props,
            current_properties: start_props,
        };

        self.animations.insert(id, animation_state);
        id
    }

    pub fn create_color_animation(
        &mut self,
        start_color: Color,
        end_color: Color,
        duration: Duration,
        easing: EasingFunction,
    ) -> u32 {
        let mut start_props = AnimatableProperties::default();
        start_props.color = start_color;
        let mut end_props = AnimatableProperties::default();
        end_props.color = end_color;

        let config = AnimationConfig {
            duration,
            easing,
            animation_type: AnimationType::Color,
            repeat: false,
            reverse: false,
        };
        self.create_animation(start_props, end_props, config)
    }

    pub fn create_scale_animation(
        &mut self,
        start_scale: f32,
        end_scale: f32,
        duration: Duration,
        easing: EasingFunction,
    ) -> u32 {
        let mut start_props = AnimatableProperties::default();
        start_props.scale_x = start_scale;
        start_props.scale_y = start_scale;
        let mut end_props = AnimatableProperties::default();
        end_props.scale_x = end_scale;
        end_props.scale_y = end_scale;

        let config = AnimationConfig {
            duration,
            easing,
            animation_type: AnimationType::Scale,
            repeat: false,
            reverse: false,
        };
        self.create_animation(start_props, end_props, config)
    }

    pub fn create_opacity_animation(
        &mut self,
        start_opacity: f32,
        end_opacity: f32,
        duration: Duration,
        easing: EasingFunction,
    ) -> u32 {
        let mut start_props = AnimatableProperties::default();
        start_props.opacity = start_opacity;
        let mut end_props = AnimatableProperties::default();
        end_props.opacity = end_opacity;

        let config = AnimationConfig {
            duration,
            easing,
            animation_type: AnimationType::Opacity,
            repeat: false,
            reverse: false,
        };
        self.create_animation(start_props, end_props, config)
    }

    pub fn create_rotation_animation(
        &mut self,
        start_rotation: f32,
        end_rotation: f32,
        duration: Duration,
        easing: EasingFunction,
    ) -> u32 {
        let mut start_props = AnimatableProperties::default();
        start_props.rotation = start_rotation;
        let mut end_props = AnimatableProperties::default();
        end_props.rotation = end_rotation;

        let config = AnimationConfig {
            duration,
            easing,
            animation_type: AnimationType::Rotation,
            repeat: false,
            reverse: false,
        };
        self.create_animation(start_props, end_props, config)
    }

    pub fn create_glow_animation(
        &mut self,
        start_intensity: f32,
        end_intensity: f32,
        start_color: Color,
        end_color: Color,
        duration: Duration,
        easing: EasingFunction,
    ) -> u32 {
        let mut start_props = AnimatableProperties::default();
        start_props.glow_intensity = start_intensity;
        start_props.glow_color = start_color;
        let mut end_props = AnimatableProperties::default();
        end_props.glow_intensity = end_intensity;
        end_props.glow_color = end_color;

        let config = AnimationConfig {
            duration,
            easing,
            animation_type: AnimationType::Glow,
            repeat: false,
            reverse: false,
        };
        self.create_animation(start_props, end_props, config)
    }

    pub fn create_morph_animation(
        &mut self,
        start_factor: f32,
        end_factor: f32,
        duration: Duration,
        easing: EasingFunction,
    ) -> u32 {
        let mut start_props = AnimatableProperties::default();
        start_props.morph_factor = start_factor;
        let mut end_props = AnimatableProperties::default();
        end_props.morph_factor = end_factor;

        let config = AnimationConfig {
            duration,
            easing,
            animation_type: AnimationType::Morph,
            repeat: false,
            reverse: false,
        };
        self.create_animation(start_props, end_props, config)
    }

    pub fn create_slide_animation(
        &mut self,
        start_x: f32,
        start_y: f32,
        end_x: f32,
        end_y: f32,
        duration: Duration,
        easing: EasingFunction,
    ) -> u32 {
        let mut start_props = AnimatableProperties::default();
        start_props.x = start_x;
        start_props.y = start_y;
        let mut end_props = AnimatableProperties::default();
        end_props.x = end_x;
        end_props.y = end_y;

        let config = AnimationConfig {
            duration,
            easing,
            animation_type: AnimationType::Slide,
            repeat: false,
            reverse: false,
        };
        self.create_animation(start_props, end_props, config)
    }

    pub fn create_pulse_animation(
        &mut self,
        min_scale: f32,
        max_scale: f32,
        duration: Duration,
        easing: EasingFunction,
    ) -> u32 {
        let mut start_props = AnimatableProperties::default();
        start_props.scale_x = min_scale;
        start_props.scale_y = min_scale;
        let mut end_props = AnimatableProperties::default();
        end_props.scale_x = max_scale;
        end_props.scale_y = max_scale;

        let config = AnimationConfig {
            duration,
            easing,
            animation_type: AnimationType::Pulse,
            repeat: true,
            reverse: true,
        };
        self.create_animation(start_props, end_props, config)
    }

    pub fn create_shake_animation(
        &mut self,
        intensity: f32,
        duration: Duration,
        easing: EasingFunction,
    ) -> u32 {
        let mut start_props = AnimatableProperties::default();
        start_props.translation_x = 0.0;
        start_props.translation_y = 0.0;
        let mut end_props = AnimatableProperties::default();
        end_props.translation_x = intensity;
        end_props.translation_y = intensity;

        let config = AnimationConfig {
            duration,
            easing,
            animation_type: AnimationType::Shake,
            repeat: true,
            reverse: true,
        };
        self.create_animation(start_props, end_props, config)
    }

    pub fn start_animation(&mut self, id: u32) -> bool {
        if let Some(animation) = self.animations.get_mut(&id) {
            animation.is_active = true;
            animation.start_time = self.current_time;
            animation.end_time =
                animation.start_time + animation.config.duration.as_millis() as u64;
            true
        } else {
            false
        }
    }

    pub fn pause_animation(&mut self, id: u32) -> bool {
        if let Some(animation) = self.animations.get_mut(&id) {
            animation.is_active = false;
            true
        } else {
            false
        }
    }

    pub fn cancel_animation(&mut self, id: u32) -> bool {
        self.animations.remove(&id).is_some()
    }

    pub fn update_animations(&mut self) {
        let mut completed_animations = Vec::new();

        for (id, animation) in self.animations.iter_mut() {
            if !animation.is_active {
                continue;
            }

            let elapsed_time = self.current_time.saturating_sub(animation.start_time);
            let total_duration = animation.config.duration.as_millis() as u64;

            if total_duration == 0 {
                animation.current_progress = 1.0;
            } else {
                animation.current_progress = (elapsed_time as f32 / total_duration as f32).min(1.0);
            }

            let eased_progress =
                Self::apply_easing_static(animation.current_progress, animation.config.easing);
            animation.current_properties = Self::interpolate_properties_static(
                animation.start_properties,
                animation.end_properties,
                eased_progress,
            );

            if animation.current_progress >= 1.0 {
                if animation.config.repeat {
                    animation.start_time = self.current_time;
                    animation.end_time = animation.start_time + total_duration;
                    animation.current_progress = 0.0;
                    if animation.config.reverse {
                        core::mem::swap(
                            &mut animation.start_properties,
                            &mut animation.end_properties,
                        );
                    }
                } else {
                    completed_animations.push(*id);
                }
            }
        }

        for id in completed_animations {
            self.animations.remove(&id);
        }
    }

    pub fn get_animation_properties(&self, id: u32) -> Option<AnimatableProperties> {
        self.animations.get(&id).map(|anim| anim.current_properties)
    }

    pub fn active_animations_count(&self) -> usize {
        self.animations.len()
    }

    pub fn clear_animations(&mut self) {
        self.animations.clear();
    }

    fn apply_easing(&self, t: f32, easing: EasingFunction) -> f32 {
        Self::apply_easing_static(t, easing)
    }

    fn interpolate_properties(
        &self,
        start: AnimatableProperties,
        end: AnimatableProperties,
        t: f32,
    ) -> AnimatableProperties {
        Self::interpolate_properties_static(start, end, t)
    }

    fn interpolate_color(&self, color1: Color, color2: Color, t: f32) -> Color {
        Self::interpolate_color_static(color1, color2, t)
    }

    fn apply_easing_static(t: f32, easing: EasingFunction) -> f32 {
        let t = t.clamp(0.0, 1.0);

        match easing {
            EasingFunction::Linear => t,
            EasingFunction::EaseIn => t * t,
            EasingFunction::EaseOut => t * (2.0 - t),
            EasingFunction::EaseInOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    -1.0 + (4.0 - 2.0 * t) * t
                }
            }
            EasingFunction::EaseInCubic => t * t * t,
            EasingFunction::EaseOutCubic => {
                let f = t - 1.0;
                f * f * f + 1.0
            }
            EasingFunction::EaseInOutCubic => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    let f = 2.0 * t - 2.0;
                    f * f * f / 2.0 + 1.0
                }
            }
            EasingFunction::EaseInQuart => t * t * t * t,
            EasingFunction::EaseOutQuart => {
                let f = t - 1.0;
                1.0 - f * f * f * f
            }
            EasingFunction::EaseInOutQuart => {
                if t < 0.5 {
                    8.0 * t * t * t * t
                } else {
                    let f = t - 1.0;
                    1.0 - 8.0 * f * f * f * f
                }
            }
            EasingFunction::EaseInQuint => t * t * t * t * t,
            EasingFunction::EaseOutQuint => {
                let f = t - 1.0;
                f * f * f * f * f + 1.0
            }
            EasingFunction::EaseInOutQuint => {
                if t < 0.5 {
                    16.0 * t * t * t * t * t
                } else {
                    let f = 2.0 * t - 2.0;
                    f * f * f * f * f / 2.0 + 1.0
                }
            }
            EasingFunction::EaseInSine => 1.0 - Self::cos((t * core::f32::consts::PI) / 2.0),
            EasingFunction::EaseOutSine => Self::sin((t * core::f32::consts::PI) / 2.0),
            EasingFunction::EaseInOutSine => -((Self::cos(t * core::f32::consts::PI) - 1.0) / 2.0),
            EasingFunction::EaseInExpo => {
                if t == 0.0 {
                    0.0
                } else {
                    Self::powf(2.0, 10.0 * (t - 1.0))
                }
            }
            EasingFunction::EaseOutExpo => {
                if t == 1.0 {
                    1.0
                } else {
                    1.0 - Self::powf(2.0, -10.0 * t)
                }
            }
            EasingFunction::EaseInOutExpo => {
                if t == 0.0 {
                    0.0
                } else if t == 1.0 {
                    1.0
                } else if t < 0.5 {
                    Self::powf(2.0, 20.0 * t - 10.0) / 2.0
                } else {
                    (2.0 - Self::powf(2.0, -20.0 * t + 10.0)) / 2.0
                }
            }
            EasingFunction::EaseInCirc => 1.0 - Self::sqrt(1.0 - t * t),
            EasingFunction::EaseOutCirc => Self::sqrt(1.0 - (t - 1.0) * (t - 1.0)),
            EasingFunction::EaseInOutCirc => {
                if t < 0.5 {
                    (1.0 - Self::sqrt(1.0 - 4.0 * t * t)) / 2.0
                } else {
                    Self::sqrt(1.0 - (-2.0 * t + 2.0) * (-2.0 * t + 2.0)) / 2.0 + 0.5
                }
            }
            EasingFunction::EaseInBack => {
                const C1: f32 = 1.70158;
                const C3: f32 = C1 + 1.0;
                C3 * t * t * t - C1 * t * t
            }
            EasingFunction::EaseOutBack => {
                const C1: f32 = 1.70158;
                const C3: f32 = C1 + 1.0;
                let f = t - 1.0;
                1.0 + C3 * f * f * f + C1 * f * f
            }
            EasingFunction::EaseInOutBack => {
                const C1: f32 = 1.70158;
                const C2: f32 = C1 * 1.525;
                if t < 0.5 {
                    ((2.0 * t) * (2.0 * t) * ((C2 + 1.0) * 2.0 * t - C2)) / 2.0
                } else {
                    let f = 2.0 * t - 2.0;
                    (f * f * ((C2 + 1.0) * f + C2) + 2.0) / 2.0
                }
            }
            EasingFunction::EaseInElastic => {
                if t == 0.0 {
                    0.0
                } else if t == 1.0 {
                    1.0
                } else {
                    const C4: f32 = (2.0 * core::f32::consts::PI) / 3.0;
                    -(Self::powf(2.0, 10.0 * t - 10.0)) * Self::sin((t * 10.0 - 10.75) * C4)
                }
            }
            EasingFunction::EaseOutElastic => {
                if t == 0.0 {
                    0.0
                } else if t == 1.0 {
                    1.0
                } else {
                    const C4: f32 = (2.0 * core::f32::consts::PI) / 3.0;
                    Self::powf(2.0, -10.0 * t) * Self::sin((t * 10.0 - 0.75) * C4) + 1.0
                }
            }
            EasingFunction::EaseInOutElastic => {
                if t == 0.0 {
                    0.0
                } else if t == 1.0 {
                    1.0
                } else if t < 0.5 {
                    const C5: f32 = (2.0 * core::f32::consts::PI) / 4.5;
                    -(Self::powf(2.0, 20.0 * t - 10.0)) * Self::sin((20.0 * t - 11.125) * C5) / 2.0
                } else {
                    const C5: f32 = (2.0 * core::f32::consts::PI) / 4.5;
                    (Self::powf(2.0, -20.0 * t + 10.0) * Self::sin((20.0 * t - 11.125) * C5) / 2.0)
                        + 1.0
                }
            }
            EasingFunction::EaseInBounce => 1.0 - Self::ease_out_bounce_static(1.0 - t),
            EasingFunction::EaseOutBounce => Self::ease_out_bounce_static(t),
            EasingFunction::EaseInOutBounce => {
                if t < 0.5 {
                    (1.0 - Self::ease_out_bounce_static(1.0 - 2.0 * t)) / 2.0
                } else {
                    (1.0 + Self::ease_out_bounce_static(2.0 * t - 1.0)) / 2.0
                }
            }
        }
    }

    fn ease_out_bounce_static(t: f32) -> f32 {
        const N1: f32 = 7.5625;
        const D1: f32 = 2.75;

        if t < 1.0 / D1 {
            N1 * t * t
        } else if t < 2.0 / D1 {
            let t = t - 1.5 / D1;
            N1 * t * t + 0.75
        } else if t < 2.5 / D1 {
            let t = t - 2.25 / D1;
            N1 * t * t + 0.9375
        } else {
            let t = t - 2.625 / D1;
            N1 * t * t + 0.984375
        }
    }

    fn interpolate_properties_static(
        start: AnimatableProperties,
        end: AnimatableProperties,
        t: f32,
    ) -> AnimatableProperties {
        AnimatableProperties {
            x: start.x + (end.x - start.x) * t,
            y: start.y + (end.y - start.y) * t,
            width: start.width + (end.width - start.width) * t,
            height: start.height + (end.height - start.height) * t,
            color: Self::interpolate_color_static(start.color, end.color, t),
            scale_x: start.scale_x + (end.scale_x - start.scale_x) * t,
            scale_y: start.scale_y + (end.scale_y - start.scale_y) * t,
            opacity: start.opacity + (end.opacity - start.opacity) * t,
            border_radius: start.border_radius + (end.border_radius - start.border_radius) * t,
            shadow_blur: start.shadow_blur + (end.shadow_blur - start.shadow_blur) * t,
            rotation: start.rotation + (end.rotation - start.rotation) * t,
            skew_x: start.skew_x + (end.skew_x - start.skew_x) * t,
            skew_y: start.skew_y + (end.skew_y - start.skew_y) * t,
            translation_x: start.translation_x + (end.translation_x - start.translation_x) * t,
            translation_y: start.translation_y + (end.translation_y - start.translation_y) * t,
            glow_intensity: start.glow_intensity + (end.glow_intensity - start.glow_intensity) * t,
            glow_color: Self::interpolate_color_static(start.glow_color, end.glow_color, t),
            morph_factor: start.morph_factor + (end.morph_factor - start.morph_factor) * t,
        }
    }

    fn interpolate_color_static(color1: Color, color2: Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);

        let (r1, g1, b1, a1) = color1.to_rgba();
        let (r2, g2, b2, a2) = color2.to_rgba();

        let r = (r1 as f32 + (r2 as f32 - r1 as f32) * t) as u8;
        let g = (g1 as f32 + (g2 as f32 - g1 as f32) * t) as u8;
        let b = (b1 as f32 + (b2 as f32 - b1 as f32) * t) as u8;
        let a = (a1 as f32 + (a2 as f32 - a1 as f32) * t) as u8;

        Color::from_rgba(r, g, b, a)
    }

    // Funciones matemáticas simples para no_std
    fn sin(x: f32) -> f32 {
        // Aproximación simple de sin usando serie de Taylor
        let x = x % (2.0 * core::f32::consts::PI);
        let x2 = x * x;
        let x3 = x2 * x;
        let x5 = x3 * x2;
        let x7 = x5 * x2;
        x - x3 / 6.0 + x5 / 120.0 - x7 / 5040.0
    }

    fn cos(x: f32) -> f32 {
        // Aproximación simple de cos usando serie de Taylor
        let x = x % (2.0 * core::f32::consts::PI);
        let x2 = x * x;
        let x4 = x2 * x2;
        let x6 = x4 * x2;
        let x8 = x6 * x2;
        1.0 - x2 / 2.0 + x4 / 24.0 - x6 / 720.0 + x8 / 40320.0
    }

    fn powf(base: f32, exp: f32) -> f32 {
        // Aproximación simple de powf usando logaritmo
        if base <= 0.0 || exp == 0.0 {
            return 1.0;
        }
        if base == 1.0 {
            return 1.0;
        }
        if exp == 1.0 {
            return base;
        }
        if exp == 2.0 {
            return base * base;
        }
        if exp == 3.0 {
            return base * base * base;
        }
        // Para otros casos, usar aproximación simple
        let mut result = 1.0;
        let steps = (exp.abs() as i32).min(10); // Limitar para evitar bucles largos
        for _ in 0..steps {
            result *= base;
        }
        if exp < 0.0 {
            1.0 / result
        } else {
            result
        }
    }

    fn sqrt(x: f32) -> f32 {
        // Aproximación simple de sqrt usando método de Newton
        if x < 0.0 {
            return 0.0;
        }
        if x == 0.0 || x == 1.0 {
            return x;
        }

        let mut guess = x / 2.0;
        for _ in 0..10 {
            // 10 iteraciones deberían ser suficientes
            let new_guess = (guess + x / guess) / 2.0;
            if (new_guess - guess).abs() < 0.001 {
                return new_guess;
            }
            guess = new_guess;
        }
        guess
    }
}
