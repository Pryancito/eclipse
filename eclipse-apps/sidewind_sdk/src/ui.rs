use crate::font_terminus_12;
use crate::{font_terminus_14, font_terminus_20};
use embedded_graphics::{
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{Polyline, PrimitiveStyleBuilder, Circle, Line, Rectangle, RoundedRectangle, CornerRadii},
    Pixel,
    text::Text,
    mono_font::MonoTextStyle,
};
use micromath::F32Ext;
use heapless::{Vec, String};

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
    pub const LOGO: &[u8] = include_bytes!("../assets/logo.bin");
}

pub const STANDARD_ICON_SIZE: u32 = 64;
pub const BUTTON_ICON_SIZE: u32 = 20;

/// Eclipse OS Color Palette - premium cosmic aesthetic, rich and elegant
pub mod colors {
    use super::Rgb888;
    pub const ACCENT_BLUE: Rgb888 = Rgb888::new(100, 230, 255);
    pub const ACCENT_CYAN: Rgb888 = Rgb888::new(0, 229, 255);
    pub const ACCENT_VIOLET: Rgb888 = Rgb888::new(180, 140, 255);
    pub const ACCENT_RED: Rgb888 = Rgb888::new(255, 95, 110);
    pub const ACCENT_GREEN: Rgb888 = Rgb888::new(100, 255, 150);
    pub const ACCENT_YELLOW: Rgb888 = Rgb888::new(255, 240, 120);
    pub const WHITE: Rgb888 = Rgb888::new(255, 255, 255);
    pub const WARM_WHITE: Rgb888 = Rgb888::new(248, 250, 255);

    pub const BACKGROUND_DEEP: Rgb888 = Rgb888::new(4, 4, 12);
    pub const PANEL_BG: Rgb888 = Rgb888::new(10, 18, 40);
    pub const TITLE_BAR_BG: Rgb888 = Rgb888::new(16, 28, 55);

    pub const GLOW_DIM: Rgb888 = Rgb888::new(0, 64, 80);
    pub const GLOW_MID: Rgb888 = Rgb888::new(0, 128, 160);
    pub const GLOW_HI: Rgb888 = Rgb888::new(0, 229, 255);

    /// Cosmic theme - electric cyan, high-tech
    pub const COSMIC_DEEP: Rgb888 = Rgb888::new(2, 2, 8);
    pub const COSMIC_MID: Rgb888 = Rgb888::new(8, 15, 35);
    pub const COSMIC_LIGHT: Rgb888 = Rgb888::new(15, 30, 65);
    pub const NEBULA_BLUE: Rgb888 = Rgb888::new(10, 40, 100);
    pub const NEBULA_PURPLE: Rgb888 = Rgb888::new(25, 20, 60);
    pub const NEBULA_CYAN: Rgb888 = Rgb888::new(0, 70, 110);
    /// Glass-like panel (premium translucent look)
    pub const GLASS_PANEL: Rgb888 = Rgb888::new(16, 24, 45);
    /// Simulated frosted glass (pre-mixed "transparent" over cosmic bg)
    pub const GLASS_FROSTED: Rgb888 = Rgb888::new(12, 20, 38);
    pub const GLASS_BORDER: Rgb888 = Rgb888::new(0, 128, 160);
    pub const GLASS_HIGHLIGHT: Rgb888 = Rgb888::new(160, 224, 255);
    /// Metallic dock - polished tech look
    pub const DOCK_METAL: Rgb888 = Rgb888::new(20, 45, 95);
    pub const DOCK_GLASS: Rgb888 = Rgb888::new(18, 55, 115);
    /// Window shadow (layered soft shadow)
    pub const SHADOW_DARK: Rgb888 = Rgb888::new(1, 2, 10);
    pub const SHADOW_MID: Rgb888 = Rgb888::new(3, 5, 18);
}

/// True if (dx, dy) relative to hex center is inside a flat-top hexagon of given size.
fn point_in_hexagon(dx: i32, dy: i32, size: i32) -> bool {
    let h = (size as f32 * 0.866) as i32;
    let half_w = size;
    let quarter = size / 2;
    let abs_dy = dy.abs();
    abs_dy <= h && dx.abs() <= half_w - (quarter * abs_dy / h.max(1))
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

/// Glowing hexagon - premium tech neon with layered glow (holographic look)
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

    // Outer halo (soft diffuse glow) - más visible
    let halo = Rgb888::new(
        (color.r() as u16 * 40 / 100).min(255) as u8,
        (color.g() as u16 * 50 / 100).min(255) as u8,
        (color.b() as u16 * 65 / 100).min(255) as u8,
    );
    let style_outer = PrimitiveStyleBuilder::new()
        .stroke_color(halo)
        .stroke_width(3)
        .build();
    Polyline::new(&get_points(size + 14)).into_styled(style_outer).draw(target)?;
    let style_mid = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(
            (color.r() as u16 * 60 / 100).min(255) as u8,
            (color.g() as u16 * 70 / 100).min(255) as u8,
            (color.b() as u16 * 85 / 100).min(255) as u8,
        ))
        .stroke_width(2)
        .build();
    Polyline::new(&get_points(size + 7)).into_styled(style_mid).draw(target)?;

    // Main hex: translucent fill + bright neon border
    let fill = Rgb888::new(
        (color.r() as u16 * 22 / 100).min(255) as u8,
        (color.g() as u16 * 35 / 100).min(255) as u8,
        (color.b() as u16 * 50 / 100).min(255) as u8,
    );
    let style_main = PrimitiveStyleBuilder::new()
        .stroke_color(color)
        .stroke_width(2)
        .fill_color(fill)
        .build();
    Polyline::new(&get_points(size)).into_styled(style_main).draw(target)?;

    // Inner hex detail (más impacto visual)
    if size >= 24 {
        let inner_sz = size / 2;
        let inner_fill = Rgb888::new(
            (color.r() as u16 * 8 / 100).min(255) as u8,
            (color.g() as u16 * 15 / 100).min(255) as u8,
            (color.b() as u16 * 25 / 100).min(255) as u8,
        );
        let inner_style = PrimitiveStyleBuilder::new()
            .stroke_color(Rgb888::new(color.r() / 2, color.g() / 2, color.b() / 2))
            .stroke_width(1)
            .fill_color(inner_fill)
            .build();
        Polyline::new(&get_points(inner_sz)).into_styled(inner_style).draw(target)?;
    }

    Ok(())
}

/// Glowing circle - estilo neon circular (alternativa a hexágono)
pub fn draw_glowing_circle<D>(
    target: &mut D,
    center: Point,
    radius: i32,
    color: Rgb888,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let halo = Rgb888::new(
        (color.r() as u16 * 40 / 100).min(255) as u8,
        (color.g() as u16 * 50 / 100).min(255) as u8,
        (color.b() as u16 * 65 / 100).min(255) as u8,
    );
    let style_outer = PrimitiveStyleBuilder::new().stroke_color(halo).stroke_width(3).build();
    let _ = Circle::with_center(center, (radius + 8) as u32).into_styled(style_outer).draw(target)?;
    let style_mid = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(
            (color.r() as u16 * 65 / 100).min(255) as u8,
            (color.g() as u16 * 75 / 100).min(255) as u8,
            (color.b() as u16 * 90 / 100).min(255) as u8,
        ))
        .stroke_width(2)
        .build();
    let _ = Circle::with_center(center, (radius + 4) as u32).into_styled(style_mid).draw(target)?;
    let fill = Rgb888::new(
        (color.r() as u16 * 22 / 100).min(255) as u8,
        (color.g() as u16 * 35 / 100).min(255) as u8,
        (color.b() as u16 * 50 / 100).min(255) as u8,
    );
    let style_main = PrimitiveStyleBuilder::new()
        .stroke_color(color)
        .stroke_width(2)
        .fill_color(fill)
        .build();
    let _ = Circle::with_center(center, radius as u32).into_styled(style_main).draw(target)?;
    Ok(())
}

/// Tamaño del logo central (600x600)
pub const LOGO_SIZE: i32 = 600;

/// Offset de muestreo (0 = imagen centrada). El centrado en pantalla se hace como en el kernel:
/// start = (screen - logo_size) / 2, centro = start + radius.
const LOGO_SRC_OFFSET_X: i32 = 0;
const LOGO_SRC_OFFSET_Y: i32 = 0;

/// Dibuja una imagen procedimental de un eclipse solar (creciente de sol).
/// La zona de la luna es transparente (no se dibuja); solo se dibuja el sol visible.
pub fn draw_eclipse_graphic<D>(
    target: &mut D,
    center: Point,
    radius: i32,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let r = radius.max(10);
    let r2_sun = r * r;
    let moon_offset = Point::new(r / 4, -r / 5);
    let moon_center = center + moon_offset;
    let moon_r = (r * 9) / 10;
    let moon_r2 = moon_r * moon_r;

    let sun_color = Rgb888::new(255, 220, 80);
    let stroke_color = Rgb888::new(255, 200, 50);

    for oy in -r..=r {
        for ox in -r..=r {
            if ox * ox + oy * oy > r2_sun {
                continue;
            }
            let px = center.x + ox;
            let py = center.y + oy;
            let dx = px - moon_center.x;
            let dy = py - moon_center.y;
            if dx * dx + dy * dy <= moon_r2 {
                continue; // Dentro de la luna: transparente (no dibujar)
            }
            let on_edge = ox * ox + oy * oy >= (r - 1) * (r - 1);
            let color = if on_edge { stroke_color } else { sun_color };
            let _ = Pixel(Point::new(px, py), color).draw(target)?;
        }
    }

    // Corona exterior (glow suave)
    let corona = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(80, 180, 255))
        .stroke_width(2)
        .build();
    let _ = Circle::with_center(center, (r + 8) as u32).into_styled(corona).draw(target)?;
    Ok(())
}

/// Draw logo 600x600 scaled and masked to circle - centered, black transparent (color-key).
pub fn draw_circular_logo_600<D>(
    target: &mut D,
    center: Point,
    display_radius: i32,
    raw_data: &[u8],
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    const SRC_SIZE: i32 = 600;
    const TRANSPARENT_THRESH: u8 = 24;

    let r2 = display_radius * display_radius;
    // Usamos el diámetro real para el mapeo (píxeles totales en el eje)
    let diameter = 2 * display_radius;

    for oy in -display_radius..display_radius {
        for ox in -display_radius..display_radius {
            // El +0.5 implícito en la comparación de radio mejora el redondeo del círculo
            if ox * ox + oy * oy >= r2 {
                continue;
            }

            // Mapeo preciso: (coordenada_relativa / diametro) * tamaño_fuente
            // Sumamos LOGO_SRC_OFFSET para el ajuste manual que definiste
            let sx = ((ox + display_radius) * SRC_SIZE) / diameter + LOGO_SRC_OFFSET_X;
            let sy = ((oy + display_radius) * SRC_SIZE) / diameter + LOGO_SRC_OFFSET_Y;

            let sx = sx.clamp(0, SRC_SIZE - 1);
            let sy = sy.clamp(0, SRC_SIZE - 1);

            let idx = (sy as usize * SRC_SIZE as usize + sx as usize) * 3;
            
            if idx + 2 >= raw_data.len() { continue; }

            let r = raw_data[idx];
            let g = raw_data[idx + 1];
            let b = raw_data[idx + 2];

            if r < TRANSPARENT_THRESH && g < TRANSPARENT_THRESH && b < TRANSPARENT_THRESH {
                continue;
            }

            // Dibujamos relativo al centro pasado por parámetro
            target.draw_iter(core::iter::once(Pixel(
                Point::new(center.x + ox, center.y + oy), 
                Rgb888::new(r, g, b)
            )))?;
        }
    }
    Ok(())
}

/// Draw icon scaled and masked to circle - respects transparency (color-key).
/// Dibuja píxel a píxel para evitar overflow de pila (sin buffer grande).
pub fn draw_circular_icon<D>(
    target: &mut D,
    center: Point,
    radius: i32,
    raw_data: &[u8],
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    const SRC_SIZE: i32 = 64;
    const TRANSPARENT_THRESH: u8 = 24;

    let r2 = radius * radius;

    for oy in -radius..=radius {
        for ox in -radius..=radius {
            if ox * ox + oy * oy > r2 {
                continue;
            }
            let sx = ((ox + radius) * SRC_SIZE) / (2 * radius);
            let sy = ((oy + radius) * SRC_SIZE) / (2 * radius);
            let sx = sx.clamp(0, SRC_SIZE - 1);
            let sy = sy.clamp(0, SRC_SIZE - 1);
            let idx = (sy as usize * SRC_SIZE as usize + sx as usize) * 3;
            if idx + 2 >= raw_data.len() {
                continue;
            }
            let r = raw_data[idx];
            let g = raw_data[idx + 1];
            let b = raw_data[idx + 2];
            if r < TRANSPARENT_THRESH && g < TRANSPARENT_THRESH && b < TRANSPARENT_THRESH {
                continue;
            }
            let _ = Pixel(Point::new(center.x + ox, center.y + oy), Rgb888::new(r, g, b)).draw(target)?;
        }
    }
    Ok(())
}

/// Minimal shadow for floating holographic feel (lighter, less offset)
pub fn draw_window_shadow<D>(
    target: &mut D,
    rect: Rectangle,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let (x, y) = (rect.top_left.x, rect.top_left.y);
    let (w, h) = (rect.size.width as i32, rect.size.height as i32);
    let r1 = Rectangle::new(Point::new(x + 4, y + 4), Size::new(w as u32, h as u32));
    let _ = r1.into_styled(PrimitiveStyleBuilder::new().fill_color(colors::SHADOW_MID).build()).draw(target)?;
    Ok(())
}

/// Window glow - holographic border (focused: strong cyan, unfocused: subtle)
pub fn draw_window_glow<D>(
    target: &mut D,
    rect: Rectangle,
    color: Rgb888,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let tl = rect.top_left;
    let sz = rect.size;
    let soft = Rgb888::new(color.r() / 4, color.g() / 4, color.b() / 3);
    let style_outer = PrimitiveStyleBuilder::new().stroke_color(soft).stroke_width(6).build();
    let _ = Rectangle::new(tl - Point::new(6, 6), sz + Size::new(12, 12)).into_styled(style_outer).draw(target)?;
    let mid = Rgb888::new(color.r() / 2, color.g() / 2, color.b() / 2);
    let style_mid = PrimitiveStyleBuilder::new().stroke_color(mid).stroke_width(2).build();
    let _ = Rectangle::new(tl - Point::new(2, 2), sz + Size::new(4, 4)).into_styled(style_mid).draw(target)?;
    Ok(())
}

/// Helper to draw a standard 64x64 icon. Black (and near-black) pixels are treated as transparent (color-key).
pub fn draw_standard_icon<D>(
    target: &mut D,
    center: Point,
    raw_data: &[u8],
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    const TRANSPARENT_THRESH: u8 = 24;
    let size = STANDARD_ICON_SIZE as i32;
    let half = size / 2;

    for y in 0..size {
        for x in 0..size {
            let idx = (y as usize * size as usize + x as usize) * 3;
            if idx + 2 >= raw_data.len() {
                continue;
            }
            let r = raw_data[idx];
            let g = raw_data[idx + 1];
            let b = raw_data[idx + 2];
            if r < TRANSPARENT_THRESH && g < TRANSPARENT_THRESH && b < TRANSPARENT_THRESH {
                continue;
            }
            let _ = Pixel(
                Point::new(center.x - half + x, center.y - half + y),
                Rgb888::new(r, g, b),
            )
            .draw(target)?;
        }
    }
    Ok(())
}

/// Draw icon scaled and masked to hexagon - fits inside hex, respects transparency.
/// Dibuja píxel a píxel para evitar overflow de pila (sin buffer grande).
/// - hex_content_size: radius of the hexagonal mask (e.g. 38 for hex_size 52)
/// - Pixels with r,g,b all < 24 are treated as transparent (color-key)
pub fn draw_hexagonal_icon<D>(
    target: &mut D,
    center: Point,
    hex_content_size: i32,
    raw_data: &[u8],
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    const SRC_SIZE: i32 = 64;
    const TRANSPARENT_THRESH: u8 = 24;

    let size = hex_content_size;

    for oy in -size..=size {
        for ox in -size..=size {
            if !point_in_hexagon(ox, oy, size) {
                continue;
            }
            let sx = ((ox + size) * SRC_SIZE) / (2 * size);
            let sy = ((oy + size) * SRC_SIZE) / (2 * size);
            let sx = sx.clamp(0, SRC_SIZE - 1);
            let sy = sy.clamp(0, SRC_SIZE - 1);
            let idx = (sy as usize * SRC_SIZE as usize + sx as usize) * 3;
            if idx + 2 >= raw_data.len() {
                continue;
            }
            let r = raw_data[idx];
            let g = raw_data[idx + 1];
            let b = raw_data[idx + 2];
            if r < TRANSPARENT_THRESH && g < TRANSPARENT_THRESH && b < TRANSPARENT_THRESH {
                continue;
            }
            let _ = Pixel(Point::new(center.x + ox, center.y + oy), Rgb888::new(r, g, b)).draw(target)?;
        }
    }
    Ok(())
}

/// Draw icon with glass card backdrop (glassmorphism)
pub fn draw_standard_icon_with_backdrop<D>(
    target: &mut D,
    center: Point,
    raw_data: &[u8],
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let pad = 12;
    let size = STANDARD_ICON_SIZE as i32 + pad * 2;
    let top_left = center - Point::new(size / 2, size / 2);
    let backdrop = Rectangle::new(top_left, Size::new(size as u32, size as u32));
    let inner = Rgb888::new(
        (colors::GLASS_PANEL.r() as u16 * 115 / 100).min(255) as u8,
        (colors::GLASS_PANEL.g() as u16 * 110 / 100).min(255) as u8,
        (colors::GLASS_PANEL.b() as u16 * 100 / 100).min(255) as u8,
    );
    let style = PrimitiveStyleBuilder::new()
        .fill_color(inner)
        .stroke_color(colors::GLASS_BORDER)
        .stroke_width(2)
        .build();
    let _ = backdrop.into_styled(style).draw(target)?;
    let highlight_rect = Rectangle::new(top_left, Size::new(size as u32, 3));
    let _ = highlight_rect.into_styled(PrimitiveStyleBuilder::new().fill_color(colors::GLASS_HIGHLIGHT).build()).draw(target)?;
    draw_standard_icon(target, center, raw_data)
}

/// Helper to draw a standard 20x20 button icon. Black (and near-black) pixels are treated as transparent (color-key).
pub fn draw_button_icon<D>(
    target: &mut D,
    top_left: Point,
    raw_data: &[u8],
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    const TRANSPARENT_THRESH: u8 = 24;
    let size = BUTTON_ICON_SIZE as i32;

    for y in 0..size {
        for x in 0..size {
            let idx = (y as usize * size as usize + x as usize) * 3;
            if idx + 2 >= raw_data.len() {
                continue;
            }
            let r = raw_data[idx];
            let g = raw_data[idx + 1];
            let b = raw_data[idx + 2];
            if r < TRANSPARENT_THRESH && g < TRANSPARENT_THRESH && b < TRANSPARENT_THRESH {
                continue;
            }
            let _ = Pixel(
                Point::new(top_left.x + x, top_left.y + y),
                Rgb888::new(r, g, b),
            )
            .draw(target)?;
        }
    }
    Ok(())
}

/// Draw button icon with hover highlight (rounded rect background when hovered)
pub fn draw_button_icon_with_hover<D>(
    target: &mut D,
    top_left: Point,
    raw_data: &[u8],
    hovered: bool,
    accent_color: Rgb888,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    if hovered {
        let pad = 2;
        let bg = Rectangle::new(
            top_left - Point::new(pad, pad),
            Size::new(BUTTON_ICON_SIZE + (pad as u32) * 2, BUTTON_ICON_SIZE + (pad as u32) * 2),
        );
        let style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(accent_color.r() / 6, accent_color.g() / 6, accent_color.b() / 4))
            .stroke_color(accent_color)
            .stroke_width(1)
            .build();
        let _ = bg.into_styled(style).draw(target)?;
    }
    draw_button_icon(target, top_left, raw_data)
}

/// Helper to draw a procedural starfield
pub fn draw_starfield<D>(target: &mut D, seed: &mut u32, offset: Point) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888> + OriginDimensions,
{
    let size = target.size();
    let w = size.width as i32;
    let h = size.height as i32;
    for _ in 0..150 {
        *seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let x = ((*seed % w as u32) as i32 + offset.x) % w;
        *seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let y = ((*seed % h as u32) as i32 + offset.y) % h;
        let brightness = (*seed % 150 + 100) as u8;
        let color = Rgb888::new(brightness, brightness, brightness + 50);
        Pixel(Point::new(x, y), color).draw(target)?;
    }
    Ok(())
}

/// Enhanced cosmic starfield: more stars (700), varied sizes (1x1, 2x2, 3x3), color variation
pub fn draw_starfield_cosmic<D>(target: &mut D, seed: &mut u32, offset: Point) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888> + OriginDimensions,
{
    let size = target.size();
    let w = size.width as i32;
    let h = size.height as i32;
    for _ in 0..900 {
        *seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let mut x = ((*seed % w as u32) as i32 + offset.x) % w;
        if x < 0 { x += w; }
        *seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let mut y = ((*seed % h as u32) as i32 + offset.y) % h;
        if y < 0 { y += h; }
        *seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let brightness = (*seed % 195 + 85) as u8;
        *seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        // Color variation: blue-white (70%), warm white (20%), slight magenta (10%)
        let color_type = *seed % 100;
        let color = if color_type < 70 {
            let blue_tint = (brightness as u16 * 125 / 100).min(255) as u8;
            Rgb888::new(brightness, brightness.saturating_add(25), blue_tint.saturating_add(35))
        } else if color_type < 90 {
            Rgb888::new(brightness, brightness.saturating_add(15), brightness.saturating_add(20))
        } else {
            Rgb888::new(brightness.saturating_add(20), brightness, (brightness as u16 * 110 / 100).min(255) as u8)
        };
        *seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        // Size variation: 1x1 (60%), 2x2 (28%), 3x3 (12%)
        let size_roll = *seed % 100;
        let star_size = if size_roll < 60 { 1 } else if size_roll < 88 { 2 } else { 3 };
        for dy in 0..star_size {
            for dx in 0..star_size {
                let px = x + dx;
                let py = y + dy;
                if px >= 0 && px < w && py >= 0 && py < h {
                    let dim = if star_size == 1 { 1.0 } else if star_size == 2 { 0.92 } else { 0.85 };
                    let r = (color.r() as f32 * dim) as u8;
                    let g = (color.g() as f32 * dim) as u8;
                    let b = (color.b() as f32 * dim) as u8;
                    Pixel(Point::new(px, py), Rgb888::new(r, g, b)).draw(target)?;
                }
            }
        }
    }
    Ok(())
}

/// Cosmic background: rich gradient, nebula blobs, vignette. Draw first, before stars.
pub fn draw_cosmic_background<D>(target: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888> + OriginDimensions,
{
    let size = target.size();
    let w = size.width as i32;
    let h = size.height as i32;
    let cx = w / 2;
    let cy = h / 2;

    // 1. Gradiente de profundidad: oscuro en bordes -> brillante en centro (radial)
    const BAND_H: i32 = 12;
    const BAND_W: i32 = 24;
    let cx_f = cx as f32;
    let cy_f = cy as f32;
    let mut y = 0i32;
    while y < h {
        let mut x = 0i32;
        while x < w {
            let dx = (x as f32 - cx_f) / w as f32;
            let dy = (y as f32 - cy_f) / h as f32;
            let d = (dx * dx + dy * dy).sqrt();
            let glow = 1.0 - d * 1.1; // More aggressive vignette
            let r = (2.0 + (glow.max(0.0)) * 22.0) as u8;
            let g = (4.0 + (glow.max(0.0)) * 36.0) as u8;
            let b = (10.0 + (glow.max(0.0)) * 75.0) as u8;
            let color = Rgb888::new(r, g, b);
            let bw = BAND_W.min(w - x);
            let bh = BAND_H.min(h - y);
            let rect = Rectangle::new(Point::new(x, y), Size::new(bw as u32, bh as u32));
            let _ = rect.into_styled(PrimitiveStyleBuilder::new().fill_color(color).build()).draw(target)?;
            x += bw;
        }
        y += BAND_H;
    }

    // 2. Nebula blobs - deeper colors
    let nebula: [(i32, i32, u32, Rgb888); 6] = [
        (w / 6, h / 3, 240, Rgb888::new(15, 45, 110)),
        (w * 5 / 6, h / 4, 300, Rgb888::new(12, 60, 130)),
        (w / 2, h * 3 / 4, 280, Rgb888::new(20, 45, 95)),
        (cx, cy, 350, Rgb888::new(10, 30, 80)),
        (w / 4, h * 2 / 3, 200, Rgb888::new(25, 60, 140)),
        (w * 3 / 4, h * 2 / 3, 180, Rgb888::new(40, 35, 90)),
    ];
    for (nx, ny, rad, color) in nebula.iter() {
        let style = PrimitiveStyleBuilder::new().fill_color(*color).build();
        let _ = Circle::with_center(Point::new(*nx, *ny), *rad).into_styled(style).draw(target)?;
    }

    // 3. Vignette (darker corners for depth and focus)
    let vw = (w as f32 * 0.35) as i32;
    let vh = (h as f32 * 0.35) as i32;
    let vc = Rgb888::new(2, 4, 14);
    let _ = Rectangle::new(Point::new(0, 0), Size::new(vw as u32, vh as u32)).into_styled(PrimitiveStyleBuilder::new().fill_color(vc).build()).draw(target)?;
    let _ = Rectangle::new(Point::new(w - vw, 0), Size::new(vw as u32, vh as u32)).into_styled(PrimitiveStyleBuilder::new().fill_color(vc).build()).draw(target)?;
    let _ = Rectangle::new(Point::new(0, h - vh), Size::new(vw as u32, vh as u32)).into_styled(PrimitiveStyleBuilder::new().fill_color(vc).build()).draw(target)?;
    let _ = Rectangle::new(Point::new(w - vw, h - vh), Size::new(vw as u32, vh as u32)).into_styled(PrimitiveStyleBuilder::new().fill_color(vc).build()).draw(target)?;

    Ok(())
}

/// Helper to draw a subtle horizontal transparent effect (no vertical lines, no cross)
pub fn draw_grid<D>(target: &mut D, color: Rgb888, spacing: u32, offset: Point) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888> + OriginDimensions,
{
    let size = target.size();
    let w = size.width as i32;
    let h = size.height as i32;
    // Color más tenue para efecto transparente/sutil
    let transparent = Rgb888::new(
        (color.r() as u16 * 45 / 100).min(255) as u8,
        (color.g() as u16 * 50 / 100).min(255) as u8,
        (color.b() as u16 * 55 / 100).min(255) as u8,
    );
    let style = PrimitiveStyleBuilder::new().stroke_color(transparent).stroke_width(1).build();
    let off_y = offset.y % spacing as i32;

    for y in (off_y..h).step_by(spacing as usize) {
        if y < 0 { continue; }
        let _ = Line::new(Point::new(0, y), Point::new(w, y)).into_styled(style).draw(target)?;
    }
    Ok(())
}

/// A grid that appears to recede into the distance (Stage 9)
pub fn draw_perspective_grid<D>(target: &mut D, color: Rgb888, counter: u64) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888> + OriginDimensions,
{
    let size = target.size();
    let w = size.width as i32;
    let h = size.height as i32;
    let vanish_y = 0; 
    let horizon_y = h / 3;
    
    // Vertical converging lines
    let num_v = 16;
    for i in 0..=num_v {
        let x_bottom = (i * w) / num_v;
        let x_top = w / 2 + (x_bottom - w / 2) / 4;
        let c = Rgb888::new(
            (color.r() as f32 * 0.4) as u8,
            (color.g() as f32 * 0.4) as u8,
            (color.b() as f32 * 0.4) as u8,
        );
        let s = PrimitiveStyleBuilder::new().stroke_color(c).stroke_width(1).build();
        let _ = Line::new(Point::new(x_bottom, h), Point::new(x_top, horizon_y))
            .into_styled(s).draw(target)?;
    }
    
    // Horizontal lines with exponential spacing
    let scroll = (counter % 120) as f32 / 120.0;
    for i in 0..12 {
        let t = (i as f32 + scroll) / 12.0;
        let y = h - (t * t * (h - horizon_y) as f32) as i32;
        if y < horizon_y { continue; }
        
        let alpha_mod = 1.0 - (i as f32 / 12.0);
        let c = Rgb888::new(
            (color.r() as f32 * alpha_mod * 0.5) as u8,
            (color.g() as f32 * alpha_mod * 0.5) as u8,
            (color.b() as f32 * alpha_mod * 0.5) as u8,
        );
        let s = PrimitiveStyleBuilder::new().stroke_color(c).stroke_width(1).build();
        let _ = Line::new(Point::new(0, y), Point::new(w, y)).into_styled(s).draw(target)?;
    }
    
    Ok(())
}

/// Technical rotating arc (holographic segment)
pub fn draw_tech_arc<D>(
    target: &mut D,
    center: Point,
    radius: u32,
    start_angle: f32,
    sweep_angle: f32,
    color: Rgb888,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let style = PrimitiveStyleBuilder::new().stroke_color(color).stroke_width(1).build();
    let mut prev_p: Option<Point> = None;
    
    for i in 0..20 {
        let a = start_angle + (sweep_angle * i as f32 / 20.0);
        let rad = a.to_radians();
        let curr_p = center + Point::new((rad.cos() * radius as f32) as i32, (rad.sin() * radius as f32) as i32);
        
        if let Some(p) = prev_p {
            let _ = Line::new(p, curr_p).into_styled(style).draw(target)?;
        }
        prev_p = Some(curr_p);
    }
    
    // Add small technical dots at ends
    if let Some(p) = prev_p {
        let _ = Circle::with_center(p, 2).into_styled(PrimitiveStyleBuilder::new().fill_color(color).build()).draw(target)?;
    }
    
    Ok(())
}

/// Global scanline overlay (Stage 9)
pub fn draw_scanlines<D>(target: &mut D, color: Rgb888) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888> + OriginDimensions,
{
    let size = target.size();
    let w = size.width as i32;
    let h = size.height as i32;
    let style = PrimitiveStyleBuilder::new().stroke_color(color).stroke_width(1).build();
    
    for y in (0..h).step_by(4) {
        let _ = Line::new(Point::new(0, y), Point::new(w, y)).into_styled(style).draw(target)?;
    }
    Ok(())
}

/// A vertical technical sidebar with bars and readouts
pub fn draw_side_monitor<D>(target: &mut D, x: i32, h: i32, counter: u64, color: Rgb888) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let style = PrimitiveStyleBuilder::new().stroke_color(colors::GLOW_DIM).stroke_width(1).build();
    let _ = Line::new(Point::new(x, 40), Point::new(x, h - 40)).into_styled(style).draw(target)?;
    
    for i in 0..15 {
        let y = 60 + (i * 40);
        if y > h - 60 { break; }
        
        let bar_w = ((counter as f32 * 0.1 + i as f32).sin().abs() * 30.0) as u32;
        let _ = Rectangle::new(Point::new(x - 35, y), Size::new(bar_w, 4))
            .into_styled(PrimitiveStyleBuilder::new().fill_color(color).build()).draw(target)?;
        
        let _ = Circle::with_center(Point::new(x, y + 2), 2).into_styled(PrimitiveStyleBuilder::new().fill_color(colors::WHITE).build()).draw(target)?;
    }
    
    Ok(())
}

/// Char size for font_terminus_14 (8x14)
const RING_FONT_W: i32 = 8;
const RING_FONT_H: i32 = 14;

/// A rotating ring of technical text (Stage 9) - font_terminus_14 + contorno oscuro
pub fn draw_technical_text_ring<D>(
    target: &mut D,
    center: Point,
    radius: u32,
    text: &str,
    counter: u64,
    color: Rgb888,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let text_style = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, color);
    let stroke_style = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, colors::BACKGROUND_DEEP);
    let chars: Vec<char, 64> = text.chars().take(64).collect();
    let num_chars = chars.len();
    let rot_phase = (counter as f32 * 0.02);

    for i in 0..num_chars {
        let angle = (i as f32 * 360.0 / num_chars as f32) + rot_phase;
        let rad = angle.to_radians();
        let cx = (rad.cos() * radius as f32) as i32;
        let cy = (rad.sin() * radius as f32) as i32;
        // Centro del glifo en la circunferencia; offset para top-left (8x14)
        let p = center + Point::new(cx - RING_FONT_W / 2, cy - RING_FONT_H / 2);

        let mut c_str = heapless::String::<4>::new();
        let _ = c_str.push(chars[i]);

        // Contorno oscuro para legibilidad (4 offsets)
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let _ = Text::new(&c_str, p + Point::new(dx, dy), stroke_style).draw(target)?;
        }
        let _ = Text::new(&c_str, p, text_style).draw(target)?;
    }

    Ok(())
}

/// Central Eclipse OS logo - prominent metallic/crystal with strong electric cyan glow
/// Animated Eclipse logo hub with rotating rings and glowing crescent
/// logo_display_radius: radio en píxeles del logo 600x600 (ej. 200)
pub fn draw_eclipse_logo<D>(
    target: &mut D,
    center: Point,
    counter: u64,
    logo_display_radius: i32,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    // 1. Eclipse dentro del círculo central (radio limitado para no invadir anillos)
    let inner_radius = 140; // Círculo central: dentro del text ring (165) y arcs (145+)
    let eclipse_r = logo_display_radius.min(inner_radius);
    let _ = draw_eclipse_graphic(target, center, eclipse_r)?;

    // 2. Rotating technical text ring
    let _ = draw_technical_text_ring(target, center, 165, "ECLIPSE-SYSTEM-KERNEL-6.X-STABLE-LINK-ACTIVE-", counter, colors::ACCENT_CYAN);

    // 3. Rotating tech decorative arcs
    let arc_rot = (counter as f32 * 0.5);
    let _ = draw_tech_arc(target, center, 180, -arc_rot * 1.5, 60.0, colors::GLOW_HI);
    let _ = draw_tech_arc(target, center, 195, arc_rot * 0.8 + 180.0, 30.0, colors::ACCENT_VIOLET);
    let _ = draw_tech_arc(target, center, 145, arc_rot * 1.2, 45.0, colors::ACCENT_CYAN);
    let rings = [
        (280u32, colors::GLOW_DIM, 1),
        (275u32, Rgb888::new(20, 60, 150), 2),
        (260u32, colors::ACCENT_CYAN, 1),
        (255u32, colors::GLOW_MID, 2),
        (240u32, colors::GLOW_HI, 2),
    ];
    for (i, (r, col, weight)) in rings.iter().enumerate() {
        let rot_offset = (counter as f32 * (0.01 + i as f32 * 0.005)).sin() * 5.0;
        let style = PrimitiveStyleBuilder::new().stroke_color(*col).stroke_width(*weight).build();
        let _ = Circle::with_center(center, (*r as f32 + rot_offset) as u32).into_styled(style).draw(target)?;
    }

    // Rotating technical ticks
    let rot_phase = (counter as f32 * 0.01);
    for angle in (0..360).step_by(5) {
        let rad = (angle as f32).to_radians() + rot_phase;
        let is_major = angle % 30 == 0;
        let brightness_mod = 0.6 + 0.4 * (rad * 2.0).sin().abs();
        let (len_start, len_end, color) = if is_major {
            (230.0, 255.0, colors::ACCENT_CYAN)
        } else {
            (235.0, 250.0, colors::GLOW_MID)
        };
        
        let r = (color.r() as f32 * brightness_mod) as u8;
        let g = (color.g() as f32 * brightness_mod) as u8;
        let b = (color.b() as f32 * brightness_mod) as u8;
        
        let p1 = center + Point::new((rad.cos() * len_start) as i32, (rad.sin() * len_start) as i32);
        let p2 = center + Point::new((rad.cos() * len_end) as i32, (rad.sin() * len_end) as i32);
        let _ = Line::new(p1, p2).into_styled(PrimitiveStyleBuilder::new().stroke_color(Rgb888::new(r, g, b)).stroke_width(1).build()).draw(target)?;
    }

    let text_glow = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, Rgb888::new(40, 120, 180));
    let text_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::ACCENT_CYAN);
    let text_stroke = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::BACKGROUND_DEEP);
    let base = center + Point::new(-45, 170);
    for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        let _ = Text::new("ECLIPSE OS", base + Point::new(dx, dy), text_stroke).draw(target)?;
    }
    let _ = Text::new("ECLIPSE OS", base + Point::new(1, 1), text_glow).draw(target)?;
    let _ = Text::new("ECLIPSE OS", base, text_style).draw(target)?;
    
    Ok(())
}

/// Compact Eclipse logo for dashboard/centers (smaller, centered)
/// Usa logo 600x600 escalado a display_radius (ej. 22)
pub fn draw_eclipse_logo_compact<D>(
    target: &mut D,
    center: Point,
    display_radius: i32,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let r = display_radius.max(20) + 55; // halo around logo
    let halo = PrimitiveStyleBuilder::new().stroke_color(Rgb888::new(20, 60, 150)).stroke_width(2).build();
    let _ = Circle::with_center(center, (r + 18) as u32).into_styled(halo).draw(target)?;
    let glow = PrimitiveStyleBuilder::new().stroke_color(colors::ACCENT_CYAN).stroke_width(2).build();
    let _ = Circle::with_center(center, (r + 5) as u32).into_styled(glow).draw(target)?;
    let fill = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(22, 55, 110))
        .stroke_color(colors::GLASS_HIGHLIGHT)
        .stroke_width(2)
        .build();
    let _ = Circle::with_center(center, r as u32).into_styled(fill).draw(target)?;
    let shadow = PrimitiveStyleBuilder::new().fill_color(colors::BACKGROUND_DEEP).build();
    let _ = Circle::with_center(center + Point::new(r / 4, 0), (r - 8) as u32).into_styled(shadow).draw(target)?;
    let _ = draw_eclipse_graphic(target, center, display_radius)?;
    let text_glow = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, Rgb888::new(40, 120, 180));
    let text_style = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, colors::ACCENT_CYAN);
    let _ = Text::new("ECLIPSE", center + Point::new(-31, r as i32 + 13), text_glow).draw(target)?;
    let _ = Text::new("ECLIPSE", center + Point::new(-32, r as i32 + 12), text_style).draw(target)?;
    Ok(())
}

/// Circular numeric widget (pilas tipo "2867", "335" - concepto)
pub fn draw_circular_numeric_widget<D>(
    target: &mut D,
    center: Point,
    radius: i32,
    value: &str,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let r = radius as u32;
    let style = PrimitiveStyleBuilder::new()
        .fill_color(colors::GLASS_PANEL)
        .stroke_color(colors::GLASS_BORDER)
        .stroke_width(2)
        .build();
    let _ = Circle::with_center(center, r * 2).into_styled(style).draw(target)?;
    let glow = PrimitiveStyleBuilder::new().stroke_color(colors::ACCENT_CYAN).stroke_width(1).build();
    let _ = Circle::with_center(center, (r + 2) as u32).into_styled(glow).draw(target)?;
    let tw = value.len() as i32 * 10;
    let val_glow = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, Rgb888::new(40, 120, 180));
    let text_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::ACCENT_CYAN);
    let _ = Text::new(value, center + Point::new(-tw / 2 + 1, -5), val_glow).draw(target)?;
    let _ = Text::new(value, center + Point::new(-tw / 2, -6), text_style).draw(target)?;
    Ok(())
}

/// Small widget card for dashboard (icon + label + value) - estilo holograma/glass
pub fn draw_dashboard_widget_card<D>(
    target: &mut D,
    pos: Point,
    w: i32,
    h: i32,
    label: &str,
    value: &str,
    icon: &[u8],
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let rect = Rectangle::new(pos, Size::new(w as u32, h as u32));
    let bg = PrimitiveStyleBuilder::new()
        .fill_color(colors::GLASS_FROSTED)
        .stroke_color(colors::GLOW_HI)
        .stroke_width(2)
        .build();
    let _ = rect.into_styled(bg).draw(target)?;
    let _ = Rectangle::new(pos + Point::new(2, 2), Size::new((w - 4) as u32, 2))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(colors::GLASS_HIGHLIGHT).build())
        .draw(target)?;
    let icon_center = pos + Point::new(w / 2, 40);
    let _ = draw_standard_icon(target, icon_center, icon);
    let lbl_x = (w - label.len() as i32 * 8) / 2;
    let lbl_y = h - 36;
    let label_pos = pos + Point::new(lbl_x, lbl_y);
    let label_glow = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, Rgb888::new(40, 120, 180));
    let label_style = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, colors::ACCENT_CYAN);
    let _ = Text::new(label, label_pos + Point::new(1, 1), label_glow).draw(target)?;
    let _ = Text::new(label, label_pos, label_style).draw(target)?;
    let val_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::WARM_WHITE);
    let _ = Text::new(value, pos + Point::new((w - value.len() as i32 * 10) / 2, h - 22), val_style).draw(target)?;
    Ok(())
}

/// A high-fidelity "Glass Card" with rounded corners, a header, and specular glow effects
pub fn draw_glass_card<D>(
    target: &mut D,
    rect: Rectangle,
    title: &str,
    color: Rgb888,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let tl = rect.top_left;
    let sz = rect.size;
    let radius = CornerRadii::new(Size::new(8, 8));
    
    // 1. Shadow/Glow base
    let shadow_rect = RoundedRectangle::new(rect, radius);
    let shadow_style = PrimitiveStyleBuilder::new()
        .stroke_color(colors::SHADOW_DARK)
        .stroke_width(4)
        .build();
    let _ = shadow_rect.into_styled(shadow_style).draw(target)?;

    // 2. Main semi-transparent panel (frosted glass)
    let bg_style = PrimitiveStyleBuilder::new()
        .fill_color(colors::GLASS_FROSTED)
        .stroke_color(colors::GLOW_DIM)
        .stroke_width(1)
        .build();
    let _ = shadow_rect.into_styled(bg_style).draw(target)?;

    // 3. Title bar area (glass gradient simulation)
    let header_h = 26u32;
    let header_rect = Rectangle::new(tl, Size::new(sz.width, header_h));
    let header_style = PrimitiveStyleBuilder::new()
        .fill_color(colors::TITLE_BAR_BG)
        .build();
    let _ = header_rect.into_styled(header_style).draw(target)?;

    // 4. Specular Top Edge (The "Crystal" look)
    let spec_style = PrimitiveStyleBuilder::new().fill_color(colors::GLASS_HIGHLIGHT).build();
    let _ = Rectangle::new(tl + Point::new(4, 1), Size::new(sz.width - 8, 1))
        .into_styled(spec_style).draw(target)?;
    
    // 5. Neon cyan separator
    let separator_style = PrimitiveStyleBuilder::new().stroke_color(color).stroke_width(1).build();
    let _ = Line::new(tl + Point::new(0, header_h as i32), tl + Point::new(sz.width as i32, header_h as i32))
        .into_styled(separator_style).draw(target)?;

    // 6. Title text
    let text_style = MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, color);
    let _ = Text::new(title, tl + Point::new(12, 17), text_style).draw(target)?;

    Ok(())
}

/// Draws a procedural waveform graph (technical/monitor style)
pub fn draw_waveform_graph<D>(
    target: &mut D,
    rect: Rectangle,
    counter: u64,
    color: Rgb888,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let tl = rect.top_left;
    let w = rect.size.width as i32;
    let h = rect.size.height as i32;
    let center_y = tl.y + h / 2;
    
    let stroke = PrimitiveStyleBuilder::new().stroke_color(color).stroke_width(1).build();
    let mut prev_p = Point::new(tl.x, center_y);

    for x in (0..w).step_by(3) {
        // Multi-layered sine for "complex" look
        let phase = (counter as f32 * 0.1) + (x as f32 * 0.05);
        let amp = (h as f32 / 3.0) * (phase.sin() * 0.7 + (phase * 2.3).cos() * 0.3);
        let curr_p = Point::new(tl.x + x, center_y + amp as i32);
        
        let _ = Line::new(prev_p, curr_p).into_styled(stroke).draw(target)?;
        prev_p = curr_p;
    }

    Ok(())
}

/// Draws a floating data packet streamer between two points
pub fn draw_data_streamer<D>(
    target: &mut D,
    start: Point,
    end: Point,
    counter: u64,
    color: Rgb888,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let dist_x = (end.x - start.x) as f32;
    let dist_y = (end.y - start.y) as f32;
    
    // 3 packets moving along the line
    for i in 0..3 {
        let t = ((counter as f32 * 0.02) + (i as f32 * 0.33)) % 1.0;
        let p = Point::new(
            start.x + (dist_x * t) as i32,
            start.y + (dist_y * t) as i32,
        );
        
        // Packet glow
        let _ = Circle::with_center(p, 2).into_styled(PrimitiveStyleBuilder::new().fill_color(color).build()).draw(target)?;
        let _ = Circle::with_center(p, 4).into_styled(PrimitiveStyleBuilder::new().stroke_color(colors::GLOW_DIM).stroke_width(1).build()).draw(target)?;
    }
    
    // Faint connection line
    let _ = Line::new(start, end)
        .into_styled(PrimitiveStyleBuilder::new().stroke_color(colors::GLOW_DIM).stroke_width(1).build())
        .draw(target)?;

    Ok(())
}

/// Draws a technical heartbeat pulse (waveform)
pub fn draw_technical_heartbeat<D>(target: &mut D, position: Point, size: Size, counter: u64, color: Rgb888) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let w = size.width as i32;
    let h = size.height as i32;
    let mut points = [Point::new(0, 0); 20];
    
    for i in 0..20usize {
        let x = i as i32 * (w / 19);
        // Base line
        let mut y = h / 2;
        
        // Pulse logic based on counter
        let phase = (counter % 60) as i32;
        let pulse_pos = i as i32 * 3;
        if (phase - pulse_pos).abs() < 5 {
            let offset = 20 - (phase - pulse_pos).abs() * 4;
            y -= offset;
        }
        
        points[i] = position + Point::new(x, y);
    }
    
    let style = PrimitiveStyleBuilder::new()
        .stroke_color(color)
        .stroke_width(1)
        .build();
    let _ = Polyline::new(&points).into_styled(style).draw(target)?;
    
    // Add glowing dots at start/end
    let _ = Circle::with_center(points[0], 2).into_styled(PrimitiveStyleBuilder::new().fill_color(color).build()).draw(target)?;
    let _ = Circle::with_center(points[19], 2).into_styled(PrimitiveStyleBuilder::new().fill_color(color).build()).draw(target)?;

    Ok(())
}

/// Draws a technical status bar (for volume, diagnostics, etc.)
pub fn draw_technical_bar<D>(target: &mut D, position: Point, size: Size, value: f32, color: Rgb888) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let border_style = PrimitiveStyleBuilder::new().stroke_color(Rgb888::new(40, 60, 100)).stroke_width(1).build();
    let _ = Rectangle::new(position, size).into_styled(border_style).draw(target)?;
    
    // Fill segments
    let segments = 20;
    let filled_segments = (value * segments as f32) as i32;
    let seg_w = (size.width - 4) / segments as u32;
    
    for i in 0..segments {
        let seg_color = if i < (filled_segments as u32) { color } else { Rgb888::new(20, 25, 45) };
        let _ = Rectangle::new(position + Point::new(2 + (i as i32 * seg_w as i32), 2), Size::new(seg_w - 1, size.height - 4))
            .into_styled(PrimitiveStyleBuilder::new().fill_color(seg_color).build())
            .draw(target)?;
    }
    
    Ok(())
}

/// Draws a scrolling technical ticker message
pub fn draw_tech_ticker<D>(target: &mut D, position: Point, width: u32, text: &str, counter: u64, color: Rgb888) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let text_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, color);
    let char_w = 10;
    let max_chars = (width / char_w as u32) as usize;
    
    if text.is_empty() { return Ok(()); }
    
    // Scroll logic
    let shift = (counter / 4 % text.len() as u64) as usize;
    let mut display_text = [0u8; 64];
    let mut idx = 0;
    
    for i in 0..max_chars.min(63) {
        let char_idx = (shift + i) % text.len();
        display_text[idx] = text.as_bytes()[char_idx];
        idx += 1;
    }
    
    if let Ok(s) = core::str::from_utf8(&display_text[..idx]) {
        let glow_color = Rgb888::new(color.r() / 3, color.g() / 3, color.b() / 2);
        let glow_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, glow_color);
        let _ = Text::new(s, position + Point::new(1, 1), glow_style).draw(target)?;
        let _ = Text::new(s, position, text_style).draw(target)?;
    }
    
    Ok(())
}

/// Draws a technical HUD-style cursor (procedural arrow with pronounced glow)
pub fn draw_hud_cursor<D>(target: &mut D, position: Point, color: Rgb888) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let p1 = position;
    let p2 = position + Point::new(15, 10);
    let p3 = position + Point::new(7, 10);
    let p4 = position + Point::new(7, 18);
    let p5 = position + Point::new(0, 12);
    let points = [p1, p2, p3, p4, p5, p1];

    // Outer halo (soft diffuse)
    let halo = Rgb888::new(color.r() / 5, color.g() / 4, color.b() / 3);
    let halo_style = PrimitiveStyleBuilder::new().stroke_color(halo).stroke_width(6).build();
    let _ = Polyline::new(&points).into_styled(halo_style).draw(target)?;
    // Mid glow
    let glow = Rgb888::new(color.r() / 3, color.g() / 2, color.b() / 2);
    let glow_style = PrimitiveStyleBuilder::new().stroke_color(glow).stroke_width(4).build();
    let _ = Polyline::new(&points).into_styled(glow_style).draw(target)?;

    // Main shape
    let style = PrimitiveStyleBuilder::new().stroke_color(color).stroke_width(2).build();
    let _ = Polyline::new(&points).into_styled(style).draw(target)?;

    // Internal glow dot (brighter)
    let _ = Circle::with_center(position + Point::new(4, 5), 3)
        .into_styled(PrimitiveStyleBuilder::new().fill_color(color).build())
        .draw(target)?;

    Ok(())
}

/// Draws a technical slot for minimized windows (glass panel style)
pub fn draw_minimized_slot<D>(target: &mut D, position: Point, title: &str, color: Rgb888) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let _ = draw_glowing_hexagon(target, position, 24, color);
    
    // Glass panel behind label
    let rect = Rectangle::new(position + Point::new(30, 2), Size::new(90, 22));
    let _ = rect.into_styled(PrimitiveStyleBuilder::new()
        .fill_color(colors::GLASS_PANEL)
        .stroke_color(colors::GLASS_BORDER)
        .stroke_width(1)
        .build()).draw(target)?;
    
    let label_glow = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, Rgb888::new(40, 120, 180));
    let label_style = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, color);
    let mut display_text = [0u8; 12];
    let len = title.len().min(11);
    display_text[..len].copy_from_slice(&title.as_bytes()[..len]);

    if let Ok(s) = core::str::from_utf8(&display_text[..len]) {
        let _ = Text::new(s, position + Point::new(36, 7), label_glow).draw(target)?;
        let _ = Text::new(s, position + Point::new(35, 6), label_style).draw(target)?;
    }
    
    // Bottom underline decoration
    let line_style = PrimitiveStyleBuilder::new().stroke_color(color).stroke_width(1).build();
    let _ = Line::new(position + Point::new(30, 10), position + Point::new(120, 10))
        .into_styled(line_style).draw(target)?;

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

        let line_style = PrimitiveStyleBuilder::new().stroke_color(colors::ACCENT_CYAN).stroke_width(1).build();
        
        // Brackets
        let _ = Line::new(self.position, self.position + Point::new(30, 0)).into_styled(line_style).draw(target)?;
        let _ = Line::new(self.position, self.position + Point::new(0, 30)).into_styled(line_style).draw(target)?;
        
        let br = self.position + Point::new(self.size.width as i32, self.size.height as i32);
        let _ = Line::new(br - Point::new(31, 1), br - Point::new(1, 1)).into_styled(line_style).draw(target)?;
        let _ = Line::new(br - Point::new(1, 31), br - Point::new(1, 1)).into_styled(line_style).draw(target)?;

        if !self.title.is_empty() {
            let label_style = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, colors::ACCENT_CYAN);
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
        
        let val_color = if self.value > 0.8 { colors::ACCENT_RED } else { colors::ACCENT_CYAN };
        
        // Draw segmented ticks
        for i in 0..20 {
            let angle = (i as f32 * 18.0 - 90.0).to_radians();
            let p1 = self.center + Point::new((angle.cos() * (self.radius as f32 - 8.0)) as i32, (angle.sin() * (self.radius as f32 - 8.0)) as i32);
            let p2 = self.center + Point::new((angle.cos() * (self.radius as f32)) as i32, (angle.sin() * (self.radius as f32)) as i32);
            
            let color = if (i as f32 / 20.0) < self.value { val_color } else { colors::GLOW_DIM };
            let _ = Line::new(p1, p2).into_styled(PrimitiveStyleBuilder::new().stroke_color(color).stroke_width(2).build()).draw(target)?;
        }

        let label_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::WHITE);
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
pub struct Terminal<'a> {
    pub position: Point,
    pub size: Size,
    pub lines: &'a [&'a str],
}

impl<'a> Widget for Terminal<'a> {
    fn draw<D>(&self, target: &mut D) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb888>,
    {
        let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 5, 10)).build();
        let _ = Rectangle::new(self.position, self.size).into_styled(bg_style).draw(target)?;

        let border_style = PrimitiveStyleBuilder::new().stroke_color(colors::GLOW_MID).stroke_width(1).build();
        let _ = Rectangle::new(self.position, self.size).into_styled(border_style).draw(target)?;

        let text_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::ACCENT_GREEN);

        for (i, line) in self.lines.iter().enumerate() {
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
        let bg_style = PrimitiveStyleBuilder::new()
            .fill_color(colors::GLASS_PANEL)
            .stroke_color(colors::GLASS_BORDER)
            .stroke_width(2)
            .build();
        let _ = Rectangle::new(self.position, self.size).into_styled(bg_style).draw(target)?;
        let _ = Rectangle::new(self.position + Point::new(2, 2), Size::new((self.size.width.saturating_sub(4)) as u32, 2))
            .into_styled(PrimitiveStyleBuilder::new().fill_color(colors::GLASS_HIGHLIGHT).build())
            .draw(target)?;

        let title_glow = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, Rgb888::new(40, 120, 180));
        let title_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::ACCENT_CYAN);
        let _ = Text::new("NOTIFICACIONES", self.position + Point::new(21, 31), title_glow).draw(target)?;
        let _ = Text::new("NOTIFICACIONES", self.position + Point::new(20, 30), title_style).draw(target)?;

        let body_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::WHITE);
        
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

/// Compute which taskbar slot (0-7) is under cursor, or None. Call when cursor_y is in taskbar area.
pub fn taskbar_slot_under_cursor(cursor_x: i32, cursor_y: i32, taskbar_width: u32, taskbar_y: i32) -> Option<usize> {
    let th = 44i32;
    if cursor_y < taskbar_y || cursor_y >= taskbar_y + th { return None; }
    let slot_w = 106i32;
    let slot_h = 32i32;
    let margin = 12i32;
    for i in 0..8usize {
        let sx = margin + i as i32 * 118;
        if cursor_x >= sx && cursor_x < sx + slot_w
            && cursor_y >= taskbar_y + 6 && cursor_y < taskbar_y + 6 + slot_h {
            return Some(i);
        }
    }
    None
}

/// Draw hover highlight on taskbar slot (stroke-only, call after Taskbar::draw when slot is hovered)
pub fn draw_taskbar_slot_hover<D>(target: &mut D, slot: usize, taskbar_y: i32, color: Rgb888) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb888>,
{
    let sx = 12 + slot as i32 * 118;
    let rect = Rectangle::new(Point::new(sx, taskbar_y + 6), Size::new(106, 32));
    let style = PrimitiveStyleBuilder::new()
        .stroke_color(color)
        .stroke_width(2)
        .build();
    let _ = rect.into_styled(style).draw(target)?;
    Ok(())
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

        // Base metallic bar - polished tech look
        let bg_style = PrimitiveStyleBuilder::new().fill_color(colors::DOCK_METAL).build();
        let _ = Rectangle::new(Point::new(0, self.y), Size::new(tw as u32, th as u32)).into_styled(bg_style).draw(target)?;

        // Top edge HUD (brillante)
        let edge_style = PrimitiveStyleBuilder::new().stroke_color(colors::GLOW_HI).stroke_width(2).build();
        let _ = Line::new(Point::new(0, self.y), Point::new(tw, self.y)).into_styled(edge_style).draw(target)?;
        let _ = Rectangle::new(Point::new(2, self.y + 2), Size::new((tw - 4) as u32, 2))
            .into_styled(PrimitiveStyleBuilder::new().fill_color(colors::GLASS_HIGHLIGHT).build())
            .draw(target)?;
        let _ = Line::new(Point::new(0, self.y + 1), Point::new(tw, self.y + 1))
            .into_styled(PrimitiveStyleBuilder::new().stroke_color(colors::ACCENT_CYAN).stroke_width(1).build())
            .draw(target)?;
        // Bottom edge sutil
        let _ = Line::new(Point::new(0, self.y + th - 1), Point::new(tw, self.y + th - 1))
            .into_styled(PrimitiveStyleBuilder::new().stroke_color(Rgb888::new(20, 50, 100)).stroke_width(1).build())
            .draw(target)?;

        // Slots con indicador activo en 0 (HOME)
        for i in 0..8 {
            let x = 12 + i * 118;
            let slot_rect = Rectangle::new(Point::new(x, self.y + 6), Size::new(106, 32));
            let (stroke, fill) = if i == 0 {
                (colors::GLOW_HI, Rgb888::new(25, 60, 120))
            } else {
                (Rgb888::new(50, 90, 150), colors::GLASS_PANEL)
            };
            let style = PrimitiveStyleBuilder::new()
                .stroke_color(stroke)
                .stroke_width(if i == 0 { 2 } else { 1 })
                .fill_color(fill)
                .build();
            let _ = slot_rect.into_styled(style).draw(target)?;
            if i == 0 {
                let _ = Rectangle::new(Point::new(x + 2, self.y + 8), Size::new(102, 1))
                    .into_styled(PrimitiveStyleBuilder::new().fill_color(colors::GLASS_HIGHLIGHT).build())
                    .draw(target)?;
            }
        }

        // Central home button (glowing circle - tech focal point)
        let btn_cx = tw / 2;
        let btn_cy = self.y + th / 2;
        
        let p_style = PrimitiveStyleBuilder::new().stroke_color(colors::ACCENT_CYAN).stroke_width(2).build();
        let _ = Line::new(Point::new(btn_cx - 100, btn_cy), Point::new(btn_cx + 100, btn_cy)).into_styled(p_style).draw(target)?;
        let _ = Line::new(Point::new(btn_cx - 40, btn_cy - 2), Point::new(btn_cx + 40, btn_cy - 2)).into_styled(p_style).draw(target)?;

        let btn_style = PrimitiveStyleBuilder::new()
            .stroke_color(colors::GLASS_HIGHLIGHT)
            .stroke_width(2)
            .fill_color(colors::DOCK_GLASS)
            .build();
        let _ = Circle::with_center(Point::new(btn_cx, btn_cy), 14).into_styled(btn_style).draw(target)?;
        
        let _ = Circle::with_center(Point::new(btn_cx, btn_cy), 6)
            .into_styled(PrimitiveStyleBuilder::new().fill_color(colors::ACCENT_CYAN).build())
            .draw(target)?;

        let label_glow = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, Rgb888::new(40, 120, 180));
        let label_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::ACCENT_CYAN);
        let _ = Text::new("ECLIPSE", Point::new(29, self.y + 29), label_glow).draw(target)?;
        let _ = Text::new("ECLIPSE", Point::new(28, self.y + 28), label_style).draw(target)?;

        Ok(())
    }

    fn size(&self) -> Size {
        Size::new(self.width, 44)
    }
}
