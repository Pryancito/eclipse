//! Widget system for Lunas desktop UI components.
//! Provides composable widgets for building dashboard and settings panels.

use crate::painter::SkiaPainter;
use crate::style_engine::StyleEngine;

/// Bounding rectangle for widget layout.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    pub fn inset(&self, padding: f32) -> Self {
        Self {
            x: self.x + padding,
            y: self.y + padding,
            w: (self.w - padding * 2.0).max(0.0),
            h: (self.h - padding * 2.0).max(0.0),
        }
    }
}

/// Events consumed by widgets.
#[derive(Debug, Clone, Copy)]
pub enum Event {
    MouseDown(f32, f32),
    MouseUp(f32, f32),
    MouseMove(f32, f32),
    KeyPress(u16),
}

/// Messages emitted by widgets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Message {
    None,
    ButtonClicked(u32),
    SliderChanged(u32, f32),
    ToggleChanged(u32, bool),
}

/// Widget trait for composable UI elements.
pub trait Widget {
    fn draw(&self, painter: &mut SkiaPainter, style: &StyleEngine, bounds: Rect);
    fn on_event(&mut self, event: Event, bounds: Rect) -> Message;
}

/// Gauge widget — displays a progress bar with label.
pub struct Gauge {
    pub label: &'static str,
    pub value: f32,
    pub color: (u8, u8, u8),
}

impl Gauge {
    pub fn new(label: &'static str, value: f32, r: u8, g: u8, b: u8) -> Self {
        Self { label, value, color: (r, g, b) }
    }
}

impl Widget for Gauge {
    fn draw(&self, painter: &mut SkiaPainter, _style: &StyleEngine, bounds: Rect) {
        // Background
        painter.fill_round_rect(bounds.x, bounds.y, bounds.w, bounds.h, 4.0,
            SkiaPainter::color(255, 255, 255, 30));

        // Fill
        let fill_w = bounds.w * (self.value / 100.0).min(1.0);
        if fill_w > 0.0 {
            let (r, g, b) = self.color;
            painter.fill_round_rect(bounds.x, bounds.y, fill_w, bounds.h, 4.0,
                SkiaPainter::color(r, g, b, 220));
        }
    }

    fn on_event(&mut self, _event: Event, _bounds: Rect) -> Message {
        Message::None
    }
}

/// Button widget — clickable rectangle with label.
pub struct Button {
    pub id: u32,
    pub label: &'static str,
    pub active: bool,
}

impl Button {
    pub fn new(id: u32, label: &'static str) -> Self {
        Self { id, label, active: false }
    }
}

impl Widget for Button {
    fn draw(&self, painter: &mut SkiaPainter, style: &StyleEngine, bounds: Rect) {
        style.draw_button(painter, bounds.x, bounds.y, bounds.w, bounds.h, self.active);
    }

    fn on_event(&mut self, event: Event, bounds: Rect) -> Message {
        if let Event::MouseDown(mx, my) = event {
            if bounds.contains(mx, my) {
                self.active = true;
                return Message::ButtonClicked(self.id);
            }
        }
        if let Event::MouseUp(_, _) = event {
            self.active = false;
        }
        Message::None
    }
}

/// Toggle widget — on/off switch.
pub struct Toggle {
    pub id: u32,
    pub enabled: bool,
}

impl Toggle {
    pub fn new(id: u32, enabled: bool) -> Self {
        Self { id, enabled }
    }
}

impl Widget for Toggle {
    fn draw(&self, painter: &mut SkiaPainter, style: &StyleEngine, bounds: Rect) {
        let bg_color = if self.enabled {
            style.accent_color
        } else {
            SkiaPainter::color(60, 65, 80, 255)
        };
        painter.fill_round_rect(bounds.x, bounds.y, bounds.w, bounds.h, bounds.h / 2.0, bg_color);

        // Knob
        let knob_r = bounds.h * 0.35;
        let knob_x = if self.enabled {
            bounds.x + bounds.w - knob_r * 2.0 - 3.0
        } else {
            bounds.x + 3.0
        };
        let knob_y = bounds.y + (bounds.h - knob_r * 2.0) / 2.0;
        painter.fill_round_rect(knob_x, knob_y, knob_r * 2.0, knob_r * 2.0, knob_r,
            SkiaPainter::color(255, 255, 255, 240));
    }

    fn on_event(&mut self, event: Event, bounds: Rect) -> Message {
        if let Event::MouseDown(mx, my) = event {
            if bounds.contains(mx, my) {
                self.enabled = !self.enabled;
                return Message::ToggleChanged(self.id, self.enabled);
            }
        }
        Message::None
    }
}

/// Column layout — stacks children vertically.
pub struct Column {
    pub spacing: f32,
}

impl Column {
    pub fn new(spacing: f32) -> Self {
        Self { spacing }
    }

    pub fn layout_bounds(&self, parent: Rect, child_count: usize) -> alloc::vec::Vec<Rect> {
        let mut bounds = alloc::vec::Vec::new();
        if child_count == 0 { return bounds; }
        let child_h = (parent.h - self.spacing * (child_count as f32 - 1.0)) / child_count as f32;
        for i in 0..child_count {
            bounds.push(Rect {
                x: parent.x,
                y: parent.y + (child_h + self.spacing) * i as f32,
                w: parent.w,
                h: child_h,
            });
        }
        bounds
    }
}

/// Row layout — arranges children horizontally.
pub struct Row {
    pub spacing: f32,
}

impl Row {
    pub fn new(spacing: f32) -> Self {
        Self { spacing }
    }

    pub fn layout_bounds(&self, parent: Rect, child_count: usize) -> alloc::vec::Vec<Rect> {
        let mut bounds = alloc::vec::Vec::new();
        if child_count == 0 { return bounds; }
        let child_w = (parent.w - self.spacing * (child_count as f32 - 1.0)) / child_count as f32;
        for i in 0..child_count {
            bounds.push(Rect {
                x: parent.x + (child_w + self.spacing) * i as f32,
                y: parent.y,
                w: child_w,
                h: parent.h,
            });
        }
        bounds
    }
}

/// Container widget — wraps content with padding.
pub struct Container {
    pub padding: f32,
}

impl Container {
    pub fn new(padding: f32) -> Self {
        Self { padding }
    }

    pub fn inner_bounds(&self, outer: Rect) -> Rect {
        outer.inset(self.padding)
    }
}

/// Sidebar widget — displays system metrics.
pub struct Sidebar {
    pub cpu_value: f32,
    pub mem_value: f32,
    pub net_value: f32,
}

impl Sidebar {
    pub fn new() -> Self {
        Self { cpu_value: 0.0, mem_value: 0.0, net_value: 0.0 }
    }

    pub fn update(&mut self, cpu: f32, mem: f32, net: f32) {
        self.cpu_value = cpu;
        self.mem_value = mem;
        self.net_value = net;
    }
}

impl Widget for Sidebar {
    fn draw(&self, painter: &mut SkiaPainter, style: &StyleEngine, bounds: Rect) {
        style.draw_panel(painter, bounds.x, bounds.y, bounds.w, bounds.h, 10.0);

        let gauge_h = 20.0;
        let spacing = 8.0;
        let mut y = bounds.y + 10.0;

        // CPU gauge
        let cpu_gauge = Gauge::new("CPU", self.cpu_value, 0, 200, 100);
        cpu_gauge.draw(painter, style, Rect::new(bounds.x + 8.0, y, bounds.w - 16.0, gauge_h));
        y += gauge_h + spacing;

        // Memory gauge
        let mem_gauge = Gauge::new("MEM", self.mem_value, 0, 150, 255);
        mem_gauge.draw(painter, style, Rect::new(bounds.x + 8.0, y, bounds.w - 16.0, gauge_h));
        y += gauge_h + spacing;

        // Network gauge
        let net_gauge = Gauge::new("NET", self.net_value, 200, 100, 255);
        net_gauge.draw(painter, style, Rect::new(bounds.x + 8.0, y, bounds.w - 16.0, gauge_h));
    }

    fn on_event(&mut self, _event: Event, _bounds: Rect) -> Message {
        Message::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rect_contains() {
        let r = Rect::new(10.0, 10.0, 100.0, 50.0);
        assert!(r.contains(50.0, 30.0));
        assert!(!r.contains(5.0, 5.0));
        assert!(!r.contains(111.0, 30.0));
    }

    #[test]
    fn test_rect_inset() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        let inner = r.inset(10.0);
        assert!((inner.x - 10.0).abs() < 0.001);
        assert!((inner.w - 80.0).abs() < 0.001);
    }

    #[test]
    fn test_column_layout() {
        let col = Column::new(4.0);
        let parent = Rect::new(0.0, 0.0, 200.0, 100.0);
        let bounds = col.layout_bounds(parent, 3);
        assert_eq!(bounds.len(), 3);
        assert!(bounds[0].y < bounds[1].y);
        assert!(bounds[1].y < bounds[2].y);
    }

    #[test]
    fn test_row_layout() {
        let row = Row::new(4.0);
        let parent = Rect::new(0.0, 0.0, 200.0, 50.0);
        let bounds = row.layout_bounds(parent, 3);
        assert_eq!(bounds.len(), 3);
        assert!(bounds[0].x < bounds[1].x);
    }

    #[test]
    fn test_button_click() {
        let mut btn = Button::new(42, "OK");
        let bounds = Rect::new(10.0, 10.0, 80.0, 30.0);
        let msg = btn.on_event(Event::MouseDown(50.0, 25.0), bounds);
        assert_eq!(msg, Message::ButtonClicked(42));
    }

    #[test]
    fn test_button_miss() {
        let mut btn = Button::new(42, "OK");
        let bounds = Rect::new(10.0, 10.0, 80.0, 30.0);
        let msg = btn.on_event(Event::MouseDown(5.0, 5.0), bounds);
        assert_eq!(msg, Message::None);
    }

    #[test]
    fn test_toggle() {
        let mut toggle = Toggle::new(1, false);
        let bounds = Rect::new(10.0, 10.0, 50.0, 24.0);
        let msg = toggle.on_event(Event::MouseDown(35.0, 22.0), bounds);
        assert_eq!(msg, Message::ToggleChanged(1, true));
        assert!(toggle.enabled);
    }

    #[test]
    fn test_gauge_draw_no_panic() {
        let gauge = Gauge::new("CPU", 50.0, 0, 200, 100);
        // Just verify construction doesn't panic
        assert!((gauge.value - 50.0).abs() < 0.001);
    }
}
