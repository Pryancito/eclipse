use embedded_graphics::{
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{Polyline, PrimitiveStyleBuilder, Circle, Line, Rectangle},
    text::Text,
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    image::ImageRaw,
};
use micromath::F32Ext;

/// Standard Icon Assets (64x64 raw RGB888)
pub mod icons {
    pub const SYSTEM: &[u8] = include_bytes!("../assets/system.bin");
    pub const APPS: &[u8] = include_bytes!("../assets/apps.bin");
    pub const FILES: &[u8] = include_bytes!("../assets/files.bin");
    pub const NETWORK: &[u8] = include_bytes!("../assets/network.bin");
    pub const CURSOR: &[u8] = include_bytes!("../assets/cursor.bin");
    
    pub const BTN_CLOSE: &[u8] = include_bytes!("../assets/btn_close.bin");
    pub const BTN_MIN: &[u8] = include_bytes!("../assets/btn_min.bin");
    pub const BTN_MAX: &[u8] = include_bytes!("../assets/btn_max.bin");
}

pub const STANDARD_ICON_SIZE: u32 = 64;
pub const BUTTON_ICON_SIZE: u32 = 20;

/// Eclipse OS Color Palette
pub mod colors {
    use super::Rgb888;
    pub const ACCENT_BLUE: Rgb888 = Rgb888::new(100, 200, 255);
    pub const ACCENT_RED: Rgb888 = Rgb888::new(255, 80, 80);
    pub const ACCENT_GREEN: Rgb888 = Rgb888::new(80, 255, 120);
    pub const ACCENT_YELLOW: Rgb888 = Rgb888::new(255, 230, 80);
    pub const WHITE: Rgb888 = Rgb888::new(255, 255, 255);
    
    pub const BACKGROUND_DEEP: Rgb888 = Rgb888::new(5, 5, 15);
    pub const PANEL_BG: Rgb888 = Rgb888::new(10, 15, 30);
    pub const TITLE_BAR_BG: Rgb888 = Rgb888::new(20, 30, 60);
    
    pub const GLOW_DIM: Rgb888 = Rgb888::new(20, 40, 100);
    pub const GLOW_MID: Rgb888 = Rgb888::new(40, 80, 180);

    /// Cosmic theme - deep space background
    pub const COSMIC_DEEP: Rgb888 = Rgb888::new(2, 4, 18);
    pub const COSMIC_MID: Rgb888 = Rgb888::new(8, 15, 45);
    pub const COSMIC_LIGHT: Rgb888 = Rgb888::new(15, 30, 70);
    pub const NEBULA_BLUE: Rgb888 = Rgb888::new(12, 22, 55);
    pub const NEBULA_PURPLE: Rgb888 = Rgb888::new(20, 12, 40);
    /// Glass-like panel (simulated translucency)
    pub const GLASS_PANEL: Rgb888 = Rgb888::new(8, 18, 45);
    pub const GLASS_BORDER: Rgb888 = Rgb888::new(60, 120, 220);
    /// Metallic dock
    pub const DOCK_METAL: Rgb888 = Rgb888::new(25, 45, 80);
    pub const DOCK_GLASS: Rgb888 = Rgb888::new(15, 35, 75);
}

/// Helper to calculate hexagon points
fn hex_points(center: Point, size: i32) -> [Point; 7] {
    let half_w = size;
    let quarter_w = size / 2;
    let h = (size as f32 * 0.866) as i32;
    [
        center + Point::new(-quarter_w, -h),
        center + Point::new(quarter_w, -h),
        center + Point::new(half_w, 0),
        center + Point::new(quarter_w, h),
        center + Point::new(-quarter_w, h),
        center + Point::new(-half_w, 0),
        center + Point::new(-quarter_w, -h),
    ]
}

/// Helper to draw a glowing hexagon (Eclipse OS standard) - cosmic glass style
pub fn draw_glowing_hexagon<D>(
    target: &mut D,
    center: Point,
    size: i32,
    color: Rgb888,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let get_points = |s: i32| hex_points(center, s);

    // Outer glow (softer)
    let style_outer = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(color.r() / 3, color.g() / 3, color.b() / 3))
        .stroke_width(1)
        .build();
    Polyline::new(&get_points(size + 6)).into_styled(style_outer).draw(target)?;

    // Mid glow
    let style_mid = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(color.r() / 2, color.g() / 2, color.b() / 2))
        .stroke_width(2)
        .build();
    Polyline::new(&get_points(size + 3)).into_styled(style_mid).draw(target)?;

    // Main hexagon - glass fill
    let fill = Rgb888::new(
        (color.r() as u16 * 12 / 100).min(255) as u8,
        (color.g() as u16 * 15 / 100).min(255) as u8,
        (color.b() as u16 * 25 / 100).min(255) as u8,
    );
    let style_main = PrimitiveStyleBuilder::new()
        .stroke_color(color)
        .stroke_width(2)
        .fill_color(fill)
        .build();
    Polyline::new(&get_points(size)).into_styled(style_main).draw(target)?;

    Ok(())
}

/// Helper to draw a standard 64x64 icon
pub fn draw_standard_icon<D>(
    target: &mut D,
    center: Point,
    raw_data: &[u8],
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let raw = ImageRaw::<Rgb888>::new(raw_data, STANDARD_ICON_SIZE);
    let top_left = center - Point::new(STANDARD_ICON_SIZE as i32 / 2, STANDARD_ICON_SIZE as i32 / 2);
    embedded_graphics::image::Image::new(&raw, top_left).draw(target)?;
    Ok(())
}

/// Helper to draw a standard 20x20 button icon
pub fn draw_button_icon<D>(
    target: &mut D,
    top_left: Point,
    raw_data: &[u8],
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let raw = ImageRaw::<Rgb888>::new(raw_data, BUTTON_ICON_SIZE);
    embedded_graphics::image::Image::new(&raw, top_left).draw(target)?;
    Ok(())
}

/// Helper to draw a procedural starfield
pub fn draw_starfield<D>(target: &mut D, seed: &mut u32) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888> + OriginDimensions,
{
    let size = target.size();
    let w = size.width as i32;
    let h = size.height as i32;
    for _ in 0..150 {
        *seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let x = (*seed % w as u32) as i32;
        *seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let y = (*seed % h as u32) as i32;
        let brightness = (*seed % 150 + 100) as u8;
        let color = Rgb888::new(brightness, brightness, brightness + 50);
        Pixel(Point::new(x, y), color).draw(target)?;
    }
    Ok(())
}

/// Enhanced cosmic starfield: more stars, varied sizes, blue-white tones
pub fn draw_starfield_cosmic<D>(target: &mut D, seed: &mut u32) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888> + OriginDimensions,
{
    let size = target.size();
    let w = size.width as i32;
    let h = size.height as i32;
    for _ in 0..400 {
        *seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let x = (*seed % w as u32) as i32;
        *seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let y = (*seed % h as u32) as i32;
        *seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let brightness = (*seed % 180 + 80) as u8;
        let blue_tint = (brightness as u16 * 120 / 100).min(255) as u8;
        let color = Rgb888::new(brightness, brightness.saturating_add(20), blue_tint.saturating_add(30));
        Pixel(Point::new(x, y), color).draw(target)?;
        // Brighter stars: draw 2x2 cluster (10% chance)
        if (*seed % 10) == 0 && x + 1 < w && y + 1 < h {
            let bright = brightness.saturating_add(60).min(255);
            let c2 = Rgb888::new(bright, bright.saturating_add(30), 255);
            Pixel(Point::new(x + 1, y), c2).draw(target)?;
            Pixel(Point::new(x, y + 1), c2).draw(target)?;
            Pixel(Point::new(x + 1, y + 1), c2).draw(target)?;
        }
    }
    Ok(())
}

/// Cosmic background: gradient + nebula blobs. Draw first, before stars.
pub fn draw_cosmic_background<D>(target: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888> + OriginDimensions,
{
    let size = target.size();
    let w = size.width as i32;
    let h = size.height as i32;

    // Vertical gradient: dark top -> subtle mid glow -> dark bottom
    const BAND_H: i32 = 24;
    let mut y = 0i32;
    while y < h {
        let t = y as f32 / h as f32;
        let mid = 0.5;
        let glow = if t < mid {
            1.0 - (mid - t) / mid * 0.7
        } else {
            1.0 - (t - mid) / mid * 0.7
        };
        let r = (2.0 + glow * 8.0) as u8;
        let g = (5.0 + glow * 15.0) as u8;
        let b = (18.0 + glow * 35.0) as u8;
        let color = Rgb888::new(r, g, b);
        let bh = BAND_H.min(h - y);
        let rect = Rectangle::new(Point::new(0, y), Size::new(w as u32, bh as u32));
        let _ = rect.into_styled(PrimitiveStyleBuilder::new().fill_color(color).build()).draw(target)?;
        y += bh;
    }

    // Nebula blobs (soft dim circles - left, right, bottom)
    let nebula: [(i32, i32, u32, Rgb888); 4] = [
        (w / 5, h / 4, 160, colors::NEBULA_BLUE),
        (w * 4 / 5, h / 3, 130, colors::NEBULA_PURPLE),
        (w / 2, h * 4 / 5, 180, colors::NEBULA_BLUE),
        (w * 3 / 4, h / 2, 100, colors::NEBULA_PURPLE),
    ];
    for (cx, cy, rad, color) in nebula.iter() {
        let style = PrimitiveStyleBuilder::new().fill_color(*color).build();
        let _ = Circle::with_center(Point::new(*cx, *cy), *rad).into_styled(style).draw(target)?;
    }

    Ok(())
}

/// Helper to draw a subtle grid
pub fn draw_grid<D>(target: &mut D, color: Rgb888, spacing: u32) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888> + OriginDimensions,
{
    let size = target.size();
    let w = size.width as i32;
    let h = size.height as i32;
    let style = PrimitiveStyleBuilder::new().stroke_color(color).stroke_width(1).build();
    for x in (0..w).step_by(spacing as usize) {
        let _ = Line::new(Point::new(x, 0), Point::new(x, h)).into_styled(style).draw(target)?;
    }
    for y in (0..h).step_by(spacing as usize) {
        let _ = Line::new(Point::new(0, y), Point::new(w, y)).into_styled(style).draw(target)?;
    }
    Ok(())
}

/// Helper to draw the central Eclipse OS logo
pub fn draw_eclipse_logo<D>(target: &mut D, center: Point) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let ring_style_outer = PrimitiveStyleBuilder::new().stroke_color(Rgb888::new(20, 40, 100)).stroke_width(1).build();
    let ring_style_inner = PrimitiveStyleBuilder::new().stroke_color(Rgb888::new(40, 80, 180)).stroke_width(1).build();
    let _ = Circle::with_center(center, 260).into_styled(ring_style_outer).draw(target)?;
    let _ = Circle::with_center(center, 250).into_styled(ring_style_inner).draw(target)?;
    let _ = Circle::with_center(center, 230).into_styled(ring_style_inner).draw(target)?;

    for angle in (0..360).step_by(5) {
        let rad = (angle as f32).to_radians();
        let is_major = angle % 30 == 0;
        let (len_start, len_end, color) = if is_major {
            (230.0, 255.0, colors::ACCENT_BLUE)
        } else {
            (235.0, 245.0, Rgb888::new(50, 100, 200))
        };
        let p1 = center + Point::new((rad.cos() * len_start) as i32, (rad.sin() * len_start) as i32);
        let p2 = center + Point::new((rad.cos() * len_end) as i32, (rad.sin() * len_end) as i32);
        let _ = Line::new(p1, p2).into_styled(PrimitiveStyleBuilder::new().stroke_color(color).stroke_width(if is_major { 2 } else { 1 }).build()).draw(target)?;
    }

    let eclipse_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(180, 240, 255))
        .stroke_width(4)
        .fill_color(Rgb888::new(10, 25, 60))
        .build();
    let _ = Circle::with_center(center, 120).into_styled(eclipse_style).draw(target)?;
    
    let shadow_style = PrimitiveStyleBuilder::new().fill_color(colors::BACKGROUND_DEEP).build();
    let _ = Circle::with_center(center + Point::new(30, 0), 110).into_styled(shadow_style).draw(target)?;

    let text_style = MonoTextStyle::new(&FONT_10X20, Rgb888::new(200, 240, 255));
    let _ = Text::new("ECLIPSE OS", center + Point::new(-50, 170), text_style).draw(target)?;
    
    Ok(())
}

/// Base trait for all UI components
pub trait Widget {
    fn draw<D>(&self, target: &mut D) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb888>;
    
    fn size(&self) -> Size;
}

/// A container with technical decorations
pub struct Panel {
    pub position: Point,
    pub size: Size,
    pub title: &'static str,
}

impl Widget for Panel {
    fn draw<D>(&self, target: &mut D) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb888>,
    {
        let bg_style = PrimitiveStyleBuilder::new().fill_color(colors::PANEL_BG).build();
        let _ = Rectangle::new(self.position, self.size).into_styled(bg_style).draw(target)?;

        let line_style = PrimitiveStyleBuilder::new().stroke_color(colors::ACCENT_BLUE).stroke_width(1).build();
        
        // Brackets
        let _ = Line::new(self.position, self.position + Point::new(30, 0)).into_styled(line_style).draw(target)?;
        let _ = Line::new(self.position, self.position + Point::new(0, 30)).into_styled(line_style).draw(target)?;
        
        let br = self.position + Point::new(self.size.width as i32, self.size.height as i32);
        let _ = Line::new(br - Point::new(31, 1), br - Point::new(1, 1)).into_styled(line_style).draw(target)?;
        let _ = Line::new(br - Point::new(1, 31), br - Point::new(1, 1)).into_styled(line_style).draw(target)?;

        if !self.title.is_empty() {
            let label_style = MonoTextStyle::new(&FONT_10X20, colors::ACCENT_BLUE);
            let _ = Text::new(self.title, self.position + Point::new(10, 20), label_style).draw(target)?;
        }

        Ok(())
    }

    fn size(&self) -> Size {
        self.size
    }
}

/// A circular technical gauge
pub struct Gauge {
    pub center: Point,
    pub radius: u32,
    pub value: f32, // 0.0 to 1.0
    pub label: &'static str,
}

impl Widget for Gauge {
    fn draw<D>(&self, target: &mut D) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb888>,
    {
        let bg_style = PrimitiveStyleBuilder::new().stroke_color(colors::GLOW_DIM).stroke_width(2).build();
        let _ = Circle::with_center(self.center, self.radius).into_styled(bg_style).draw(target)?;
        
        let val_color = if self.value > 0.8 { colors::ACCENT_RED } else { colors::ACCENT_BLUE };
        
        // Draw segmented ticks
        for i in 0..20 {
            let angle = (i as f32 * 18.0 - 90.0).to_radians();
            let p1 = self.center + Point::new((angle.cos() * (self.radius as f32 - 8.0)) as i32, (angle.sin() * (self.radius as f32 - 8.0)) as i32);
            let p2 = self.center + Point::new((angle.cos() * (self.radius as f32)) as i32, (angle.sin() * (self.radius as f32)) as i32);
            
            let color = if (i as f32 / 20.0) < self.value { val_color } else { colors::GLOW_DIM };
            let _ = Line::new(p1, p2).into_styled(PrimitiveStyleBuilder::new().stroke_color(color).stroke_width(2).build()).draw(target)?;
        }

        let label_style = MonoTextStyle::new(&FONT_10X20, colors::WHITE);
        let _ = Text::new(self.label, self.center + Point::new(-(self.label.len() as i32 * 5), (self.radius + 20) as i32), label_style).draw(target)?;

        let val_pct = (self.value * 100.0) as i32;
        // Simple numeric display (placeholder for proper formatting)
        let _ = Text::new("OK", self.center + Point::new(-10, 5), label_style).draw(target)?;

        Ok(())
    }

    fn size(&self) -> Size {
        Size::new(self.radius * 2, self.radius * 2 + 40)
    }
}

/// Alignment primitives
pub enum Align {
    Start,
    Center,
    End,
}

pub fn position_widget(parent: Rectangle, child_size: Size, h: Align, v: Align) -> Point {
    let x = match h {
        Align::Start => parent.top_left.x,
        Align::Center => parent.top_left.x + (parent.size.width as i32 - child_size.width as i32) / 2,
        Align::End => parent.top_left.x + parent.size.width as i32 - child_size.width as i32,
    };
    let y = match v {
        Align::Start => parent.top_left.y,
        Align::Center => parent.top_left.y + (parent.size.height as i32 - child_size.height as i32) / 2,
        Align::End => parent.top_left.y + parent.size.height as i32 - child_size.height as i32,
    };
    Point::new(x, y)
}

/// A high-fidelity terminal mockup widget
pub struct Terminal {
    pub position: Point,
    pub size: Size,
}

impl Widget for Terminal {
    fn draw<D>(&self, target: &mut D) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb888>,
    {
        let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 5, 10)).build();
        let _ = Rectangle::new(self.position, self.size).into_styled(bg_style).draw(target)?;

        let border_style = PrimitiveStyleBuilder::new().stroke_color(colors::GLOW_MID).stroke_width(1).build();
        let _ = Rectangle::new(self.position, self.size).into_styled(border_style).draw(target)?;

        let text_style = MonoTextStyle::new(&FONT_10X20, colors::ACCENT_GREEN);
        let lines = [
            "root@eclipse:~# systemctl status",
            "STATUS: OPTIMAL",
            "UPTIME: 00:42:15",
            "SESSIONS: 4 ACTIVE",
            "root@eclipse:~# _",
        ];

        for (i, line) in lines.iter().enumerate() {
            let _ = Text::new(line, self.position + Point::new(15, 35 + i as i32 * 25), text_style).draw(target)?;
        }

        Ok(())
    }

    fn size(&self) -> Size {
        self.size
    }
}

/// A notification entry
#[derive(Clone, Copy)]
pub struct Notification {
    pub title: &'static str,
    pub body: &'static str,
    pub icon_type: u8, // 0=Info, 1=Warn, 2=Error
}

/// Slide-in notification panel
pub struct NotificationPanel<'a> {
    pub position: Point,
    pub size: Size,
    pub notifications: &'a [Notification],
}

impl<'a> Widget for NotificationPanel<'a> {
    fn draw<D>(&self, target: &mut D) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb888>,
    {
        let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(5, 10, 25)).build();
        let _ = Rectangle::new(self.position, self.size).into_styled(bg_style).draw(target)?;
        
        // Technical side border
        let border_style = PrimitiveStyleBuilder::new().stroke_color(colors::GLOW_MID).stroke_width(1).build();
        let _ = Line::new(self.position, self.position + Point::new(0, self.size.height as i32)).into_styled(border_style).draw(target)?;

        let title_style = MonoTextStyle::new(&FONT_10X20, colors::ACCENT_BLUE);
        let _ = Text::new("NOTIFICACIONES", self.position + Point::new(20, 30), title_style).draw(target)?;

        let body_style = MonoTextStyle::new(&FONT_10X20, colors::WHITE);
        
        for (i, n) in self.notifications.iter().enumerate().take(5) {
            let y_off = 70 + (i as i32 * 80);
            let color = match n.icon_type {
                1 => colors::ACCENT_YELLOW,
                2 => colors::ACCENT_RED,
                _ => colors::ACCENT_BLUE,
            };
            
            // Side indicator
            let _ = Rectangle::new(self.position + Point::new(10, y_off), Size::new(5, 60))
                .into_styled(PrimitiveStyleBuilder::new().fill_color(color).build())
                .draw(target)?;
                
            let _ = Text::new(n.title, self.position + Point::new(25, y_off + 20), title_style).draw(target)?;
            let _ = Text::new(n.body, self.position + Point::new(25, y_off + 45), body_style).draw(target)?;
        }

        Ok(())
    }

    fn size(&self) -> Size {
        self.size
    }
}

/// Bottom taskbar widget - cosmic dock style (metallic + translucent panels)
pub struct Taskbar {
    pub width: u32,
    pub y: i32,
    pub active_app: Option<&'static str>,
}

impl Widget for Taskbar {
    fn draw<D>(&self, target: &mut D) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb888>,
    {
        let tw = self.width as i32;
        let th: i32 = 44;

        // Base metallic bar
        let bg_style = PrimitiveStyleBuilder::new().fill_color(colors::DOCK_METAL).build();
        let _ = Rectangle::new(Point::new(0, self.y), Size::new(tw as u32, th as u32)).into_styled(bg_style).draw(target)?;

        // Top highlight (glass edge)
        let edge_style = PrimitiveStyleBuilder::new().stroke_color(colors::GLASS_BORDER).stroke_width(1).build();
        let _ = Line::new(Point::new(0, self.y), Point::new(tw, self.y)).into_styled(edge_style).draw(target)?;

        // Translucent panels for slots
        for i in 0..8 {
            let x = 12 + i * 118;
            let slot_rect = Rectangle::new(Point::new(x, self.y + 6), Size::new(106, 32));
            let style = if i == 0 {
                PrimitiveStyleBuilder::new()
                    .stroke_color(colors::ACCENT_BLUE)
                    .stroke_width(1)
                    .fill_color(colors::DOCK_GLASS)
                    .build()
            } else {
                PrimitiveStyleBuilder::new()
                    .stroke_color(Rgb888::new(35, 55, 95))
                    .stroke_width(1)
                    .fill_color(colors::GLASS_PANEL)
                    .build()
            };
            let _ = slot_rect.into_styled(style).draw(target)?;
        }

        // Central home button (glowing circle)
        let btn_cx = tw / 2;
        let btn_cy = self.y + th / 2;
        let btn_style = PrimitiveStyleBuilder::new()
            .stroke_color(colors::ACCENT_BLUE)
            .stroke_width(2)
            .fill_color(colors::DOCK_GLASS)
            .build();
        let _ = Circle::with_center(Point::new(btn_cx, btn_cy), 14).into_styled(btn_style).draw(target)?;

        let label_style = MonoTextStyle::new(&FONT_10X20, colors::ACCENT_BLUE);
        let _ = Text::new("ECLIPSE", Point::new(28, self.y + 28), label_style).draw(target)?;

        Ok(())
    }

    fn size(&self) -> Size {
        Size::new(self.width, 44)
    }
}
