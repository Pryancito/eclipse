//! Theming engine for Lunas desktop.
//! Defines the visual style for all UI components.

use crate::painter::SkiaPainter;
use tiny_skia::Color;

pub struct StyleEngine {
    pub panel_bg: Color,
    pub border_color: Color,
    pub accent_color: Color,
    pub text_color: Color,
    pub text_dim_color: Color,
    pub corner_radius: f32,
    pub animation_speed: f32,
    pub dark_mode: bool,
}

impl StyleEngine {
    pub fn new() -> Self {
        Self {
            panel_bg: SkiaPainter::color(15, 18, 35, 200),
            border_color: SkiaPainter::color(255, 255, 255, 25),
            accent_color: SkiaPainter::color(0, 128, 255, 255),
            text_color: SkiaPainter::color(220, 230, 240, 255),
            text_dim_color: SkiaPainter::color(100, 120, 160, 255),
            corner_radius: 12.0,
            animation_speed: 1.0,
            dark_mode: true,
        }
    }

    pub fn draw_panel(&self, painter: &mut SkiaPainter, x: f32, y: f32, w: f32, h: f32, radius: f32) {
        painter.fill_round_rect(x, y, w, h, radius, self.panel_bg);
        painter.stroke_round_rect(x, y, w, h, radius, 1.0, self.border_color);
    }

    pub fn draw_button(&self, painter: &mut SkiaPainter, x: f32, y: f32, w: f32, h: f32, active: bool) {
        let bg = if active { self.accent_color } else { SkiaPainter::color(255, 255, 255, 20) };
        painter.fill_round_rect(x, y, w, h, 8.0, bg);
    }

    pub fn draw_card(&self, painter: &mut SkiaPainter, x: f32, y: f32, w: f32, h: f32) {
        painter.fill_round_rect(x, y, w, h, self.corner_radius, SkiaPainter::color(20, 25, 45, 220));
        painter.stroke_round_rect(x, y, w, h, self.corner_radius, 1.0, self.border_color);
    }

    pub fn draw_input_field(&self, painter: &mut SkiaPainter, x: f32, y: f32, w: f32, h: f32, focused: bool) {
        let border = if focused { self.accent_color } else { self.border_color };
        painter.fill_round_rect(x, y, w, h, 6.0, SkiaPainter::color(25, 30, 50, 240));
        painter.stroke_round_rect(x, y, w, h, 6.0, 1.5, border);
    }

    pub fn draw_separator(&self, painter: &mut SkiaPainter, x: f32, y: f32, w: f32) {
        painter.draw_line(x, y, x + w, y, 1.0, self.border_color);
    }
}
