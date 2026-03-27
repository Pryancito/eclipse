use crate::stylus::{Widget, Rect, Message, Event};
use crate::painter::SkiaPainter;
use crate::style_engine::StyleEngine;

pub struct Gauge {
    pub value: f32,
}

impl Gauge {
    pub fn new(value: f32) -> Self {
        Self { value }
    }
}

impl Widget for Gauge {
    fn draw(&self, painter: &mut SkiaPainter, _style_engine: &StyleEngine, bounds: Rect) {
        painter.fill_round_rect(bounds.x, bounds.y, bounds.w, bounds.h, 4.0, SkiaPainter::color(255, 255, 255, 30));
        let bar_w = bounds.w * (self.value / 100.0);
        painter.fill_round_rect(bounds.x, bounds.y, bar_w, bounds.h, 4.0, SkiaPainter::color(0, 128, 255, 200));
    }
}

pub struct Button {
    pub id: u32,
    pub label: &'static str,
}

impl Button {
    pub fn new(id: u32, label: &'static str) -> Self {
        Self { id, label }
    }
}

impl Widget for Button {
    fn draw(&self, painter: &mut SkiaPainter, style_engine: &StyleEngine, bounds: Rect) {
        style_engine.draw_button(painter, bounds.x, bounds.y, bounds.w, bounds.h, false);
    }
    fn on_event(&mut self, event: Event, _bounds: Rect) -> Message {
        if let Event::MouseDown { .. } = event {
            return Message::ButtonClicked(self.id);
        }
        Message::None
    }
}

pub struct ControlCenter {}
impl ControlCenter { pub fn new() -> Self { Self {} } }
impl Widget for ControlCenter {
    fn draw(&self, _p: &mut SkiaPainter, _se: &StyleEngine, _b: Rect) {}
}

pub struct WindowSwitcher {}
impl WindowSwitcher { pub fn new(_windows: alloc::vec::Vec<crate::compositor::ShellWindow>, _idx: usize) -> Self { Self {} } }
impl Widget for WindowSwitcher {
    fn draw(&self, _p: &mut SkiaPainter, _se: &StyleEngine, _b: Rect) {}
}

pub struct PowerManager {}
impl PowerManager { pub fn new() -> Self { Self {} } }
impl Widget for PowerManager {
    fn draw(&self, _p: &mut SkiaPainter, _se: &StyleEngine, _b: Rect) {}
}

pub struct Greeter { pub username: &'static str }
impl Greeter { pub fn new() -> Self { Self { username: "moebius" } } }
impl Widget for Greeter {
    fn draw(&self, _p: &mut SkiaPainter, _se: &StyleEngine, _b: Rect) {}
}

pub struct SessionDialog {}
impl SessionDialog { pub fn new() -> Self { Self {} } }
impl Widget for SessionDialog {
    fn draw(&self, _p: &mut SkiaPainter, _se: &StyleEngine, _b: Rect) {}
}
