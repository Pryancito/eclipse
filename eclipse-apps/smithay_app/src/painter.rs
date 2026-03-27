use tiny_skia::*;

pub struct SkiaPainter<'a> {
    pub pixmap: PixmapMut<'a>,
}

impl<'a> SkiaPainter<'a> {
    pub fn new(data: &'a mut [u8], width: u32, height: u32) -> Option<Self> {
        let pixmap = PixmapMut::from_bytes(data, width, height)?;
        Some(Self { pixmap })
    }

    pub fn color(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color::from_rgba8(r, g, b, a)
    }

    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        let rect = Rect::from_xywh(x, y, w, h).unwrap();
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        self.pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }

    pub fn fill_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Color) {
        if let Some(path) = self.create_round_rect_path(x, y, w, h, radius) {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            self.pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }

    pub fn stroke_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, thickness: f32, color: Color) {
        if let Some(path) = self.create_round_rect_path(x, y, w, h, radius) {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            let mut stroke = Stroke::default();
            stroke.width = thickness;
            self.pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    pub fn draw_shadow_advanced(&mut self, _x: f32, _y: f32, _w: f32, _h: f32, _radius: f32, _spread: f32, _color: Color) {
        // Simple shadow: just a larger translucent rect for now
        self.fill_round_rect(_x - _spread, _y + _spread, _w + _spread * 2.0, _h + _spread * 2.0, _radius + _spread, _color);
    }

    pub fn fill_gradient_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color_start: Color, _color_end: Color) {
        // Simple version: just use start color for now
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
        // Mock blur for now - in a real implementation we would use a kernel
        self.fill_rect(x, y, w, h, Color::from_rgba8(255, 255, 255, 20));
    }

    // Helper for easier rectangle drawing
    pub fn draw_rect_color(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        self.fill_rect(x, y, w, h, color);
    }
}
