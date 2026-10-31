//! Menú contextual dibujado como overlay del cromo (popup opaco).

use embedded_graphics::mono_font::ascii::FONT_6X10;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use tiny_skia::{Color, Paint, Pixmap, Rect, Transform};

use crate::chrome::{self, RectI};
use crate::look::Rgb;

pub const ROW_H: i32 = 22;
pub const MENU_W: i32 = 220;
const PAD_X: i32 = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Popup {
    pub x: i32,
    pub y: i32,
    pub items: Vec<Item>,
}

impl Popup {
    pub fn height(&self) -> i32 {
        self.items
            .iter()
            .map(|i| if i.separator { 8 } else { ROW_H })
            .sum::<i32>()
            .max(ROW_H)
            + 4
    }

    pub fn width(&self) -> i32 {
        MENU_W
    }

    pub fn bounds(&self) -> RectI {
        RectI {
            x: self.x,
            y: self.y,
            w: self.width(),
            h: self.height(),
        }
    }

    /// Ajusta el popup para que quepa en la ventana.
    pub fn clamp_to(mut self, win_w: i32, win_h: i32) -> Self {
        let w = self.width();
        let h = self.height();
        if self.x + w > win_w {
            self.x = (win_w - w).max(0);
        }
        if self.y + h > win_h {
            self.y = (win_h - h).max(0);
        }
        self.x = self.x.max(0);
        self.y = self.y.max(0);
        self
    }

    /// Índice de ítem accionable bajo `(x,y)` en coords de ventana.
    pub fn hit(&self, x: i32, y: i32) -> Option<usize> {
        let b = self.bounds();
        if !b.contains(x, y) {
            return None;
        }
        let mut cy = self.y + 2;
        for (i, item) in self.items.iter().enumerate() {
            let h = if item.separator { 8 } else { ROW_H };
            if y >= cy && y < cy + h {
                if item.separator || !item.enabled {
                    return None;
                }
                return Some(i);
            }
            cy += h;
        }
        None
    }
}

/// Cromo normal, o cromo + overlay a tamaño de ventana si hay popup.
pub fn render(view: &chrome::View<'_>, win_h: i32, popup: Option<&Popup>) -> chrome::Pixels {
    match popup {
        Some(p) => render_menu_overlay(view, win_h, p),
        None => chrome::render(view),
    }
}

fn render_menu_overlay(view: &chrome::View<'_>, win_h: i32, popup: &Popup) -> chrome::Pixels {
    let scale = view.scale.max(1);
    let chrome_px = chrome::render(view);
    let width = (view.width.max(1) * scale) as u32;
    let height = (win_h.max(chrome::HEIGHT) * scale) as u32;
    let mut pixmap = Pixmap::new(width, height).expect("menu pixmap");
    pixmap.fill(Color::from_rgba8(0, 0, 0, 0));

    let strip_h = (chrome::HEIGHT * scale) as usize;
    let stride = (width as usize) * 4;
    let src = &chrome_px.argb;
    let dst = pixmap.data_mut();
    let copy_rows = strip_h.min(chrome_px.height as usize).min(height as usize);
    for row in 0..copy_rows {
        let s = row * (chrome_px.width as usize) * 4;
        let d = row * stride;
        let n = stride.min(src.len().saturating_sub(s));
        if d + n <= dst.len() && s + n <= src.len() {
            // chrome_px is already ARGB; pixmap wants RGBA for tiny-skia draws.
            // Copy ARGB→RGBA into pixmap for the strip, then convert all at end.
            for col in 0..(n / 4) {
                let si = s + col * 4;
                let di = d + col * 4;
                dst[di] = src[si + 2];
                dst[di + 1] = src[si + 1];
                dst[di + 2] = src[si];
                dst[di + 3] = src[si + 3];
            }
        }
    }

    let pal = view.look.palette();
    let bx = popup.x * scale;
    let by = popup.y * scale;
    let bw = popup.width() * scale;
    let bh = popup.height() * scale;
    fill_rect(
        &mut pixmap,
        bx,
        by,
        bw,
        bh,
        Color::from_rgba8(pal.field.r, pal.field.g, pal.field.b, 240),
    );
    stroke_rect(
        &mut pixmap,
        bx,
        by,
        bw,
        bh,
        Color::from_rgba8(pal.accent.r, pal.accent.g, pal.accent.b, 255),
        scale,
    );

    let mut cy = popup.y + 2;
    for item in &popup.items {
        if item.separator {
            let y = (cy + 3) * scale;
            fill_rect(
                &mut pixmap,
                bx + 4 * scale,
                y,
                bw - 8 * scale,
                scale.max(1),
                Color::from_rgba8(pal.dim.r, pal.dim.g, pal.dim.b, 200),
            );
            cy += 8;
            continue;
        }
        cy += ROW_H;
    }

    let mut argb = Vec::with_capacity(pixmap.data().len());
    rgba_to_argb(pixmap.data(), &mut argb);
    let mut target = ArgbTarget {
        data: &mut argb,
        width,
        height,
    };
    let mut cy = popup.y + 2;
    for item in &popup.items {
        if item.separator {
            cy += 8;
            continue;
        }
        let color = if item.enabled { pal.text } else { pal.dim };
        label(
            &mut target,
            (popup.x + PAD_X) * scale,
            (cy + 15) * scale,
            &item.label,
            color,
        );
        cy += ROW_H;
    }
    chrome::Pixels {
        argb,
        width: width as i32,
        height: height as i32,
    }
}

fn fill_rect(pixmap: &mut Pixmap, x: i32, y: i32, w: i32, h: i32, color: Color) {
    if w <= 0 || h <= 0 {
        return;
    }
    let Some(rect) = Rect::from_xywh(x as f32, y as f32, w as f32, h as f32) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = false;
    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
}

fn stroke_rect(pixmap: &mut Pixmap, x: i32, y: i32, w: i32, h: i32, color: Color, scale: i32) {
    let t = scale.max(1);
    fill_rect(pixmap, x, y, w, t, color);
    fill_rect(pixmap, x, y + h - t, w, t, color);
    fill_rect(pixmap, x, y, t, h, color);
    fill_rect(pixmap, x + w - t, y, t, h, color);
}

fn rgba_to_argb(src: &[u8], dst: &mut Vec<u8>) {
    dst.clear();
    dst.reserve(src.len());
    for px in src.chunks_exact(4) {
        dst.push(px[2]);
        dst.push(px[1]);
        dst.push(px[0]);
        dst.push(px[3]);
    }
}

struct ArgbTarget<'a> {
    data: &'a mut [u8],
    width: u32,
    height: u32,
}

impl Dimensions for ArgbTarget<'_> {
    fn bounding_box(&self) -> embedded_graphics::primitives::Rectangle {
        embedded_graphics::primitives::Rectangle::new(
            Point::zero(),
            Size::new(self.width, self.height),
        )
    }
}

impl DrawTarget for ArgbTarget<'_> {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(p, c) in pixels {
            if p.x < 0 || p.y < 0 {
                continue;
            }
            let x = p.x as u32;
            let y = p.y as u32;
            if x >= self.width || y >= self.height {
                continue;
            }
            let i = ((y * self.width + x) * 4) as usize;
            if i + 3 < self.data.len() {
                self.data[i] = c.b();
                self.data[i + 1] = c.g();
                self.data[i + 2] = c.r();
                // No pisar alpha 0 del overlay transparente.
                if self.data[i + 3] == 0 {
                    self.data[i + 3] = 255;
                }
            }
        }
        Ok(())
    }
}

fn label(target: &mut ArgbTarget<'_>, x: i32, y: i32, text: &str, rgb: Rgb) {
    let style = MonoTextStyle::new(&FONT_6X10, Rgb888::new(rgb.r, rgb.g, rgb.b));
    let _ = Text::new(text, Point::new(x, y), style).draw(target);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Popup {
        Popup {
            x: 10,
            y: 20,
            items: vec![
                Item {
                    label: "Atrás".into(),
                    enabled: true,
                    separator: false,
                },
                Item {
                    label: "".into(),
                    enabled: false,
                    separator: true,
                },
                Item {
                    label: "Recargar".into(),
                    enabled: true,
                    separator: false,
                },
            ],
        }
    }

    #[test]
    fn hit_finds_enabled_rows() {
        let p = sample();
        assert_eq!(p.hit(20, 25), Some(0));
        assert_eq!(p.hit(20, 20 + 2 + ROW_H + 4), None);
        assert_eq!(p.hit(20, 20 + 2 + ROW_H + 8 + 2), Some(2));
        assert_eq!(p.hit(0, 0), None);
    }

    #[test]
    fn clamp_keeps_menu_on_screen() {
        let p = Popup {
            x: 900,
            y: 900,
            items: sample().items,
        }
        .clamp_to(800, 600);
        assert!(p.x + p.width() <= 800);
        assert!(p.y + p.height() <= 600);
    }
}
