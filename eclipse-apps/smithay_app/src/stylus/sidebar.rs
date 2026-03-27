use crate::stylus::{Widget, Rect};
use crate::painter::SkiaPainter;
use crate::style_engine::StyleEngine;

pub struct Sidebar {
    pub cpu: f32,
    pub mem: f32,
    pub net: f32,
}

impl Sidebar {
    pub fn new(cpu: f32, mem: f32, net: f32) -> Self {
        Self { cpu, mem, net }
    }
}

impl Widget for Sidebar {
    fn draw(&self, painter: &mut SkiaPainter, style_engine: &StyleEngine, bounds: Rect) {
        style_engine.draw_panel(painter, bounds.x, bounds.y, bounds.w, bounds.h, 0.0);
    }
}
