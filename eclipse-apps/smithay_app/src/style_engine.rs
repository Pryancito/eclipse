use crate::painter::SkiaPainter;
use tiny_skia::Color;

pub struct StyleEngine {
    pub panel_bg: Color,
    pub border_color: Color,
    pub accent_color: Color,
    pub corner_radius: f32,
    pub animation_speed: f32,
}

impl StyleEngine {
    pub fn new() -> Self {
        Self {
            panel_bg: SkiaPainter::color(20, 25, 45, 180),
            border_color: SkiaPainter::color(255, 255, 255, 30),
            accent_color: SkiaPainter::color(0, 128, 255, 255),
            corner_radius: 12.0,
            animation_speed: 1.0,
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
}
