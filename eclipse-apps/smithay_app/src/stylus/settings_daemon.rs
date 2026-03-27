use crate::style_engine::StyleEngine;

pub struct SettingsDaemon {
    pub dark_mode: bool,
    pub animation_scale: f32,
}

impl SettingsDaemon {
    pub fn new() -> Self {
        Self { dark_mode: true, animation_scale: 1.0 }
    }

    pub fn apply_to_style(&self, style: &mut StyleEngine) {
        style.animation_speed = self.animation_scale;
        // Apply theme colors based on dark_mode...
    }
}
