//! Drawing primitives using tiny-skia for Lunas desktop.

use tiny_skia::*;

/// tiny-skia (highp) usa `movaps` con offsets fijos sobre RSP; sin RSP%16==0 en este marco puede #GP(13).
#[repr(align(16))]
struct TinySkiaRspAlign([u8; 16]);

#[inline]
fn tiny_skia_rsp_guard() -> TinySkiaRspAlign {
    TinySkiaRspAlign([0; 16])
}

pub struct SkiaPainter<'a> {
    pub pixmap: PixmapMut<'a>,
}

impl<'a> SkiaPainter<'a> {
    pub fn new(data: &'a mut [u8], width: u32, height: u32) -> Option<Self> {
        let _rsp = tiny_skia_rsp_guard();
        let pixmap = PixmapMut::from_bytes(data, width, height)?;
        Some(Self { pixmap })
    }

    pub fn color(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color::from_rgba8(r, g, b, a)
    }

    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        let _rsp = tiny_skia_rsp_guard();
        if let Some(rect) = Rect::from_xywh(x, y, w, h) {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            self.pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        }
    }

    pub fn fill_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Color) {
        let _rsp = tiny_skia_rsp_guard();
        if let Some(path) = self.create_round_rect_path(x, y, w, h, radius) {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            self.pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }

    pub fn stroke_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, thickness: f32, color: Color) {
        let _rsp = tiny_skia_rsp_guard();
        if let Some(path) = self.create_round_rect_path(x, y, w, h, radius) {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            let mut stroke = Stroke::default();
            stroke.width = thickness;
            self.pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    pub fn draw_shadow_advanced(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, spread: f32, color: Color) {
        self.fill_round_rect(x - spread, y + spread, w + spread * 2.0, h + spread * 2.0, radius + spread, color);
    }

    pub fn fill_gradient_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color_start: Color, _color_end: Color) {
        self.fill_round_rect(x, y, w, h, radius, color_start);
    }

    fn create_round_rect_path(&self, x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
        let mut pb = PathBuilder::new();
        pb.move_to(x + r, y);
        pb.line_to(x + w - r, y);
        pb.quad_to(x + w, y, x + w, y + r);
        pb.line_to(x + w, y + h - r);
        pb.quad_to(x + w, y + h, x + w - r, y + h);
        pb.line_to(x + r, y + h);
        pb.quad_to(x, y + h, x, y + h - r);
        pb.line_to(x, y + r);
        pb.quad_to(x, y, x + r, y);
        pb.close();
        pb.finish()
    }

    pub fn draw_line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, thickness: f32, color: Color) {
        let _rsp = tiny_skia_rsp_guard();
        let mut pb = PathBuilder::new();
        pb.move_to(x1, y1);
        pb.line_to(x2, y2);
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(color);
            let mut stroke = Stroke::default();
            stroke.width = thickness;
            self.pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    pub fn apply_blur(&mut self, x: f32, y: f32, w: f32, h: f32, _radius: u8) {
        self.fill_rect(x, y, w, h, Color::from_rgba8(255, 255, 255, 20));
    }

    pub fn draw_rect_color(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        self.fill_rect(x, y, w, h, color);
    }
}
