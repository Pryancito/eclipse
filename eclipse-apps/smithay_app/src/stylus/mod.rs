pub mod widgets;
pub mod sidebar;
pub mod settings_daemon;

use crate::painter::SkiaPainter;
use crate::style_engine::StyleEngine;

#[derive(Clone, Copy, Debug)]
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
}

pub enum Event {
    MouseMove { x: f32, y: f32 },
    MouseDown { x: f32, y: f32 },
    MouseUp { x: f32, y: f32 },
}

pub enum Message {
    None,
    ButtonClicked(u32),
    ValueChanged(u32, f32),
    ToggleLauncher,
    ToggleSidebar,
    WindowSelected(usize),
    FileSelected(usize),
    NavigateUp,
}

pub trait Widget {
    fn draw(&self, painter: &mut SkiaPainter, style_engine: &StyleEngine, bounds: Rect);
    fn on_event(&mut self, _event: Event, _bounds: Rect) -> Message {
        Message::None
    }
}

pub struct Column {
    children: alloc::vec::Vec<alloc::boxed::Box<dyn Widget>>,
    spacing: f32,
}

impl Column {
    pub fn new() -> Self {
        Self { children: alloc::vec::Vec::new(), spacing: 0.0 }
    }
    pub fn spacing(mut self, s: f32) -> Self {
        self.spacing = s;
        self
    }
    pub fn push<W: Widget + 'static>(mut self, child: W) -> Self {
        self.children.push(alloc::boxed::Box::new(child));
        self
    }
}

impl Widget for Column {
    fn draw(&self, painter: &mut SkiaPainter, style_engine: &StyleEngine, bounds: Rect) {
        let mut curr_y = bounds.y;
        for child in &self.children {
            // Very simple vertical layout: each child gets its natural height (mocked as 40 for now or similar)
            // In a real toolkit this would involve multi-pass layout.
            let child_h = 40.0; 
            child.draw(painter, style_engine, Rect::new(bounds.x, curr_y, bounds.w, child_h));
            curr_y += child_h + self.spacing;
        }
    }
}

pub struct Row {
    children: alloc::vec::Vec<alloc::boxed::Box<dyn Widget>>,
    spacing: f32,
}

impl Row {
    pub fn new() -> Self {
        Self { children: alloc::vec::Vec::new(), spacing: 0.0 }
    }
    pub fn spacing(mut self, s: f32) -> Self {
        self.spacing = s;
        self
    }
    pub fn push<W: Widget + 'static>(mut self, child: W) -> Self {
        self.children.push(alloc::boxed::Box::new(child));
        self
    }
}

impl Widget for Row {
    fn draw(&self, painter: &mut SkiaPainter, style_engine: &StyleEngine, bounds: Rect) {
        let mut curr_x = bounds.x;
        let child_w = if !self.children.is_empty() {
             (bounds.w - (self.children.len() as f32 - 1.0) * self.spacing) / self.children.len() as f32
        } else {
            0.0
        };
        for child in &self.children {
            child.draw(painter, style_engine, Rect::new(curr_x, bounds.y, child_w, bounds.h));
            curr_x += child_w + self.spacing;
        }
    }
}

pub struct Container {
    child: Option<alloc::boxed::Box<dyn Widget>>,
    padding: f32,
}

impl Container {
    pub fn new() -> Self {
        Self { child: None, padding: 0.0 }
    }
    pub fn padding(mut self, p: f32) -> Self {
        self.padding = p;
        self
    }
    pub fn child<W: Widget + 'static>(mut self, child: W) -> Self {
        self.child = Some(alloc::boxed::Box::new(child));
        self
    }
}

impl Widget for Container {
    fn draw(&self, painter: &mut SkiaPainter, style_engine: &StyleEngine, bounds: Rect) {
        if let Some(child) = &self.child {
            let padded = Rect::new(
                bounds.x + self.padding,
                bounds.y + self.padding,
                bounds.w - self.padding * 2.0,
                bounds.h - self.padding * 2.0,
            );
            child.draw(painter, style_engine, padded);
        }
    }
}
