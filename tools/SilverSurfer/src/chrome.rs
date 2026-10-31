//! The toolbar drawn into a wl_shm buffer. The page never paints this strip:
//! it is a subsurface of its own, so Servo's GL window starts below it.

use embedded_graphics::mono_font::ascii::FONT_6X10;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::text::Text;
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use crate::location::SchemeHint;
use crate::look::{Look, Rgb};

pub const TAB_H: i32 = 28;
pub const TOOL_H: i32 = 36;
pub const HEIGHT: i32 = TAB_H + TOOL_H;

const PAD: i32 = 8;
const BTN: i32 = 24;
const GAP: i32 = 4;
const SCHEME_W: i32 = 16;
const GPU_W: i32 = 28;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RectI {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl RectI {
    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    Tab(usize),
    Close(usize),
    NewTab,
    Back,
    Forward,
    Reload,
    Location,
}

#[derive(Clone, Debug)]
pub struct Layout {
    pub tabs: Vec<RectI>,
    pub closes: Vec<RectI>,
    pub new_tab: RectI,
    pub back: RectI,
    pub forward: RectI,
    pub reload: RectI,
    pub scheme: RectI,
    pub location: RectI,
    pub gpu: RectI,
}

pub fn layout(width: i32, tabs: usize) -> Layout {
    let width = width.max(1);
    let tabs = tabs.max(1);
    let y_tab = 4;
    let tab_h = TAB_H - 8;
    let plus = RectI {
        x: width - PAD - BTN,
        y: y_tab,
        w: BTN,
        h: tab_h,
    };
    let usable = (plus.x - PAD - GAP).max(1);
    // Tabs stop before the new-tab button. The preferred width is 28..160;
    // past that they shrink, and the gap goes away before a tab gets narrower
    // than its close box.
    let mut gap = GAP;
    let mut each = (usable - gap * (tabs as i32 - 1)) / tabs as i32;
    if each < 16 {
        gap = 0;
        each = usable / tabs as i32;
    }
    let each = each.clamp(1, 160);
    let mut tab_rects = Vec::with_capacity(tabs);
    let mut closes = Vec::with_capacity(tabs);
    let mut x = PAD;
    for _ in 0..tabs {
        let rect = RectI {
            x,
            y: y_tab,
            w: each,
            h: tab_h,
        };
        // Narrower than the tab, or a click would close it instead of
        // selecting it. Below 20px there is no room for a separate box.
        let close_w = if rect.w >= 20 { 12 } else { 0 };
        closes.push(RectI {
            x: rect.x + rect.w - close_w,
            y: rect.y,
            w: close_w,
            h: rect.h,
        });
        tab_rects.push(rect);
        x += each + gap;
    }
    let y_tool = TAB_H + (TOOL_H - BTN) / 2;
    let back = RectI {
        x: PAD,
        y: y_tool,
        w: BTN,
        h: BTN,
    };
    let forward = RectI {
        x: back.x + BTN + GAP,
        y: y_tool,
        w: BTN,
        h: BTN,
    };
    let reload = RectI {
        x: forward.x + BTN + GAP,
        y: y_tool,
        w: BTN,
        h: BTN,
    };
    let scheme_x = reload.x + BTN + GAP * 2;
    let scheme = RectI {
        x: scheme_x,
        y: y_tool,
        w: SCHEME_W,
        h: BTN,
    };
    let loc_x = scheme.x + scheme.w + GAP;
    let gpu = RectI {
        x: (width - PAD - GPU_W).max(loc_x),
        y: y_tool,
        w: GPU_W,
        h: BTN,
    };
    let loc_w = (gpu.x - GAP - loc_x).max(1);
    Layout {
        tabs: tab_rects,
        closes,
        new_tab: plus,
        back,
        forward,
        reload,
        scheme,
        location: RectI {
            x: loc_x,
            y: y_tool,
            w: loc_w,
            h: BTN,
        },
        gpu,
    }
}

pub fn hit(width: i32, tabs: usize, x: i32, y: i32) -> Option<Hit> {
    if y < 0 || y >= HEIGHT || x < 0 || x >= width {
        return None;
    }
    let lay = layout(width, tabs);
    for (i, rect) in lay.closes.iter().enumerate() {
        if rect.contains(x, y) {
            return Some(Hit::Close(i));
        }
    }
    for (i, rect) in lay.tabs.iter().enumerate() {
        if rect.contains(x, y) {
            return Some(Hit::Tab(i));
        }
    }
    if lay.new_tab.contains(x, y) {
        return Some(Hit::NewTab);
    }
    if lay.back.contains(x, y) {
        return Some(Hit::Back);
    }
    if lay.forward.contains(x, y) {
        return Some(Hit::Forward);
    }
    if lay.reload.contains(x, y) {
        return Some(Hit::Reload);
    }
    if lay.scheme.contains(x, y) || lay.location.contains(x, y) {
        return Some(Hit::Location);
    }
    None
}

pub struct View<'a> {
    pub width: i32,
    pub look: Look,
    pub titles: &'a [String],
    /// Parallel to `titles`: the tab is mid-load.
    pub loading: &'a [bool],
    pub active: usize,
    pub location: &'a str,
    pub location_focused: bool,
    /// Caret, in characters, used only while the bar is focused.
    pub location_cursor: usize,
    /// Sufijo de autocompletado del historial (solo barra enfocada).
    pub location_suggest: &'a str,
    /// La barra actúa como buscar-en-página (Ctrl+F).
    pub find_mode: bool,
    pub scheme: SchemeHint,
    /// `true` = página por EGL; `false` = blit software → wl_shm.
    pub gpu: bool,
    pub can_back: bool,
    pub can_forward: bool,
    /// Output scale. The buffer is `width * scale` by `HEIGHT * scale`.
    pub scale: i32,
}

/// ARGB8888, little-endian, premultiplied, ready for wl_shm.
pub struct Pixels {
    pub argb: Vec<u8>,
    pub width: i32,
    pub height: i32,
}

pub fn render(view: &View<'_>) -> Pixels {
    let scale = view.scale.max(1);
    let width = (view.width.max(1) * scale) as u32;
    let height = (HEIGHT * scale) as u32;
    let mut pixmap = Pixmap::new(width, height).expect("chrome pixmap");
    let pal = view.look.palette();
    pixmap.fill(color(pal.panel));
    let lay = layout(view.width.max(1), view.titles.len().max(1));
    let titles: &[String] = if view.titles.is_empty() {
        &[]
    } else {
        view.titles
    };
    let s = |r: RectI| scale_rect(r, scale);
    for (i, rect) in lay.tabs.iter().enumerate() {
        let on = i == view.active;
        fill(
            &mut pixmap,
            s(*rect),
            if on { pal.field } else { pal.panel },
        );
        if on {
            fill(
                &mut pixmap,
                s(RectI {
                    x: rect.x,
                    y: rect.y + rect.h - 2,
                    w: rect.w,
                    h: 2,
                }),
                pal.accent,
            );
        }
        icon_close(&mut pixmap, *rect, scale, pal.dim);
    }
    fill(&mut pixmap, s(lay.new_tab), pal.field);
    fill(&mut pixmap, s(lay.back), pal.field);
    fill(&mut pixmap, s(lay.forward), pal.field);
    fill(&mut pixmap, s(lay.reload), pal.field);
    fill(&mut pixmap, s(lay.scheme), pal.field);
    fill(&mut pixmap, s(lay.location), pal.field);
    fill(&mut pixmap, s(lay.gpu), pal.field);
    if view.location_focused {
        stroke_scaled(&mut pixmap, lay.location, pal.accent, scale);
    }
    if view.gpu {
        fill(
            &mut pixmap,
            s(RectI {
                x: lay.gpu.x,
                y: lay.gpu.y + lay.gpu.h - 2,
                w: lay.gpu.w,
                h: 2,
            }),
            pal.accent,
        );
    }
    if view.scheme == SchemeHint::Https {
        fill(
            &mut pixmap,
            s(RectI {
                x: lay.scheme.x,
                y: lay.scheme.y + lay.scheme.h - 2,
                w: lay.scheme.w,
                h: 2,
            }),
            pal.accent,
        );
    }

    let back_color = if view.can_back { pal.text } else { pal.dim };
    let forward_color = if view.can_forward { pal.text } else { pal.dim };
    icon_chevron(&mut pixmap, lay.back, scale, back_color, true);
    icon_chevron(&mut pixmap, lay.forward, scale, forward_color, false);
    icon_reload(&mut pixmap, lay.reload, scale, pal.text);
    icon_plus(&mut pixmap, lay.new_tab, scale, pal.text);

    let mut argb = Vec::with_capacity(pixmap.data().len());
    rgba_premul_to_argb(pixmap.data(), &mut argb);

    let mut target = ArgbTarget {
        data: &mut argb,
        width,
        height,
    };
    let text = pal.text;
    let dim = pal.dim;
    for (i, rect) in lay.tabs.iter().enumerate() {
        let raw = titles.get(i).map(String::as_str).unwrap_or("Nueva pestaña");
        let loading = view.loading.get(i).copied().unwrap_or(false);
        let title = if loading {
            format!("…{raw}")
        } else {
            raw.to_string()
        };
        let room = ((rect.w - 20) / 6).max(1) as usize;
        label_scaled(
            &mut target,
            rect.x + 6,
            rect.y + 14,
            &clip(&title, room),
            text,
            scale,
        );
    }
    let scheme_color = if view.scheme == SchemeHint::Https {
        pal.accent
    } else {
        dim
    };
    label_scaled(
        &mut target,
        lay.scheme.x + 5,
        lay.scheme.y + 16,
        view.scheme.badge(),
        scheme_color,
        scale,
    );
    let gpu_label = if view.gpu { "GL" } else { "SW" };
    let gpu_color = if view.gpu { pal.accent } else { dim };
    label_scaled(
        &mut target,
        lay.gpu.x + 6,
        lay.gpu.y + 16,
        gpu_label,
        gpu_color,
        scale,
    );
    let placeholder = if view.find_mode {
        "Buscar en la página"
    } else {
        "Buscar o escribir una dirección"
    };
    let loc = if view.location.is_empty() {
        placeholder
    } else {
        view.location
    };
    let loc_color = if view.location.is_empty() && !view.location_focused {
        dim
    } else {
        text
    };
    let room = (lay.location.w / 6).max(1) as usize;
    let shown = if view.location_focused {
        location_line(loc, room, view.location_cursor)
    } else {
        clip(loc, room)
    };
    label_scaled(
        &mut target,
        lay.location.x + 8,
        lay.location.y + 16,
        &shown,
        loc_color,
        scale,
    );
    // Ghost del historial: mono FONT_6X10 → 6 CSS px por carácter.
    if view.location_focused
        && !view.find_mode
        && !view.location_suggest.is_empty()
        && view.location_cursor == view.location.chars().count()
    {
        let typed_chars = view.location.chars().count().min(room.saturating_sub(1));
        let ghost = clip(view.location_suggest, room.saturating_sub(typed_chars + 1));
        if !ghost.is_empty() {
            label_scaled(
                &mut target,
                lay.location.x + 8 + (typed_chars as i32 + 1) * 6,
                lay.location.y + 16,
                &ghost,
                dim,
                scale,
            );
        }
    }
    Pixels {
        argb,
        width: width as i32,
        height: height as i32,
    }
}

/// The slice of the bar that ends at the caret, plus a `|`.
fn location_line(text: &str, room: usize, cursor: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let room = room.max(1);
    if room == 1 {
        return "|".into();
    }
    let body = room - 1;
    let start = cursor.saturating_sub(body);
    let end = (start + body).min(chars.len());
    let mut out: String = chars[start..cursor].iter().collect();
    out.push('|');
    out.extend(chars[cursor..end].iter().copied());
    out
}

fn clip(text: &str, chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i + 1 >= chars && text.chars().nth(i + 1).is_some() {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn color(c: Rgb) -> Color {
    Color::from_rgba8(c.r, c.g, c.b, 255)
}

fn scale_rect(rect: RectI, scale: i32) -> RectI {
    RectI {
        x: rect.x * scale,
        y: rect.y * scale,
        w: rect.w * scale,
        h: rect.h * scale,
    }
}

fn fill(pixmap: &mut Pixmap, rect: RectI, rgb: Rgb) {
    let Some(r) = Rect::from_xywh(rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32)
    else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color_rgba8(rgb.r, rgb.g, rgb.b, 255);
    pixmap.fill_rect(r, &paint, Transform::identity(), None);
}

fn stroke_scaled(pixmap: &mut Pixmap, rect: RectI, rgb: Rgb, scale: i32) {
    let t = scale.max(1);
    let r = scale_rect(rect, t);
    fill(
        pixmap,
        RectI {
            x: r.x,
            y: r.y,
            w: r.w,
            h: t,
        },
        rgb,
    );
    fill(
        pixmap,
        RectI {
            x: r.x,
            y: r.y + r.h - t,
            w: r.w,
            h: t,
        },
        rgb,
    );
    fill(
        pixmap,
        RectI {
            x: r.x,
            y: r.y,
            w: t,
            h: r.h,
        },
        rgb,
    );
    fill(
        pixmap,
        RectI {
            x: r.x + r.w - t,
            y: r.y,
            w: t,
            h: r.h,
        },
        rgb,
    );
}

fn stroke_path(pixmap: &mut Pixmap, path: tiny_skia::Path, rgb: Rgb, width: f32) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(rgb.r, rgb.g, rgb.b, 255);
    paint.anti_alias = true;
    let stroke = Stroke {
        width,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn icon_chevron(pixmap: &mut Pixmap, btn: RectI, scale: i32, rgb: Rgb, left: bool) {
    let s = scale.max(1) as f32;
    let cx = (btn.x + btn.w / 2) as f32 * s;
    let cy = (btn.y + btn.h / 2) as f32 * s;
    let dx = 4.0 * s * if left { -1.0 } else { 1.0 };
    let dy = 5.0 * s;
    let mut pb = PathBuilder::new();
    pb.move_to(cx - dx, cy - dy);
    pb.line_to(cx + dx, cy);
    pb.line_to(cx - dx, cy + dy);
    if let Some(path) = pb.finish() {
        stroke_path(pixmap, path, rgb, 1.6 * s);
    }
}

fn icon_reload(pixmap: &mut Pixmap, btn: RectI, scale: i32, rgb: Rgb) {
    let s = scale.max(1) as f32;
    let cx = (btn.x + btn.w / 2) as f32 * s;
    let cy = (btn.y + btn.h / 2) as f32 * s;
    let r = 6.0 * s;
    let mut pb = PathBuilder::new();
    // Open arc: from ~40° to ~320°.
    let steps = 18;
    for i in 0..=steps {
        let t = 0.35 + (std::f32::consts::TAU - 0.9) * (i as f32 / steps as f32);
        let x = cx + r * t.cos();
        let y = cy + r * t.sin();
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    if let Some(path) = pb.finish() {
        stroke_path(pixmap, path, rgb, 1.5 * s);
    }
    let mut arrow = PathBuilder::new();
    let tip_x = cx + r * 0.35f32.cos();
    let tip_y = cy + r * 0.35f32.sin();
    arrow.move_to(tip_x - 3.0 * s, tip_y - 1.0 * s);
    arrow.line_to(tip_x, tip_y);
    arrow.line_to(tip_x - 1.0 * s, tip_y + 3.5 * s);
    if let Some(path) = arrow.finish() {
        stroke_path(pixmap, path, rgb, 1.4 * s);
    }
}

fn icon_plus(pixmap: &mut Pixmap, btn: RectI, scale: i32, rgb: Rgb) {
    let s = scale.max(1);
    let cx = btn.x + btn.w / 2;
    let cy = btn.y + btn.h / 2;
    let arm = 5;
    fill(
        pixmap,
        scale_rect(
            RectI {
                x: cx - arm,
                y: cy - 1,
                w: arm * 2,
                h: 2,
            },
            s,
        ),
        rgb,
    );
    fill(
        pixmap,
        scale_rect(
            RectI {
                x: cx - 1,
                y: cy - arm,
                w: 2,
                h: arm * 2,
            },
            s,
        ),
        rgb,
    );
}

fn icon_close(pixmap: &mut Pixmap, tab: RectI, scale: i32, rgb: Rgb) {
    let s = scale.max(1) as f32;
    let cx = (tab.x + tab.w - 8) as f32 * s;
    let cy = (tab.y + tab.h / 2) as f32 * s;
    let d = 3.5 * s;
    let mut a = PathBuilder::new();
    a.move_to(cx - d, cy - d);
    a.line_to(cx + d, cy + d);
    let mut b = PathBuilder::new();
    b.move_to(cx + d, cy - d);
    b.line_to(cx - d, cy + d);
    if let Some(path) = a.finish() {
        stroke_path(pixmap, path, rgb, 1.3 * s);
    }
    if let Some(path) = b.finish() {
        stroke_path(pixmap, path, rgb, 1.3 * s);
    }
}

fn label_scaled(target: &mut ArgbTarget<'_>, x: i32, y: i32, text: &str, rgb: Rgb, scale: i32) {
    let scale = scale.max(1);
    // Draw into a 1× logical buffer then stamp scale×scale blocks so glyphs
    // stay sharp on HiDPI instead of nearest-neighbour upscaling the strip.
    let style = MonoTextStyle::new(&FONT_6X10, Rgb888::new(rgb.r, rgb.g, rgb.b));
    let w = (text.chars().count() as i32 * 6 + 2).max(1) as u32;
    let h = 12u32;
    let mut tiny = vec![0u8; (w * h * 4) as usize];
    {
        let mut scratch = ArgbTarget {
            data: &mut tiny,
            width: w,
            height: h,
        };
        let _ = Text::new(text, Point::new(0, 10), style).draw(&mut scratch);
    }
    for py in 0..h as i32 {
        for px in 0..w as i32 {
            let i = ((py as u32 * w + px as u32) * 4) as usize;
            if tiny[i + 3] == 0 {
                continue;
            }
            let color = Rgb888::new(tiny[i + 2], tiny[i + 1], tiny[i]);
            for dy in 0..scale {
                for dx in 0..scale {
                    let _ = Pixel(
                        Point::new((x + px) * scale + dx, (y - 10 + py) * scale + dy),
                        color,
                    )
                    .draw(target);
                }
            }
        }
    }
}

/// Content rectangle in device pixels. At least one pixel on each side: Servo
/// rejects an empty rendering context, and a window shorter than the chrome
/// still has a page to attach.
pub fn content_px(logical_w: i32, logical_h: i32, scale: i32) -> (u32, u32) {
    let scale = scale.max(1);
    let width = logical_w.max(1) * scale;
    let height = (logical_h - HEIGHT).max(1) * scale;
    (width as u32, height as u32)
}

/// Little-endian ARGB from an RGBA8888 image, row by row. `dst_stride` is the
/// destination row in bytes. Rows that do not fit are dropped.
pub fn blit_rgba(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst: &mut [u8],
    dst_w: u32,
    dst_stride: usize,
    dst_y: u32,
) {
    let row_bytes = (src_w.min(dst_w) as usize) * 4;
    for row in 0..src_h {
        let y = dst_y + row;
        let src_off = (row as usize) * (src_w as usize) * 4;
        let dst_off = (y as usize) * dst_stride;
        if src_off + row_bytes > src.len() || dst_off + row_bytes > dst.len() {
            break;
        }
        for px in 0..(row_bytes / 4) {
            let s = src_off + px * 4;
            let d = dst_off + px * 4;
            dst[d] = src[s + 2];
            dst[d + 1] = src[s + 1];
            dst[d + 2] = src[s];
            dst[d + 3] = src[s + 3];
        }
    }
}

/// Premultiplied RGBA (tiny-skia) to little-endian ARGB (wl_shm).
pub fn rgba_premul_to_argb(src: &[u8], dst: &mut Vec<u8>) {
    dst.clear();
    dst.reserve(src.len());
    for px in src.chunks_exact(4) {
        dst.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
}

struct ArgbTarget<'a> {
    data: &'a mut [u8],
    width: u32,
    height: u32,
}

impl OriginDimensions for ArgbTarget<'_> {
    fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

impl DrawTarget for ArgbTarget<'_> {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            if point.x < 0 || point.y < 0 {
                continue;
            }
            let (x, y) = (point.x as u32, point.y as u32);
            if x >= self.width || y >= self.height {
                continue;
            }
            let i = ((y * self.width + x) * 4) as usize;
            self.data[i] = color.b();
            self.data[i + 1] = color.g();
            self.data[i + 2] = color.r();
            self.data[i + 3] = 255;
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        self.draw_iter(area.points().map(|p| Pixel(p, color)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_location_bar_is_below_the_tabs_and_the_close_box_wins() {
        let lay = layout(800, 2);
        assert!(lay.tabs[0].y < HEIGHT);
        assert!(lay.location.y >= TAB_H);
        let close = lay.closes[0];
        assert_eq!(
            hit(800, 2, close.x + 1, close.y + 1),
            Some(Hit::Close(0))
        );
        assert_eq!(
            hit(800, 2, lay.location.x + 4, lay.location.y + 4),
            Some(Hit::Location)
        );
        assert_eq!(hit(800, 2, 1, HEIGHT), None);
    }

    #[test]
    fn swizzle_exchanges_red_and_blue() {
        let mut dst = Vec::new();
        rgba_premul_to_argb(&[10, 20, 30, 255], &mut dst);
        assert_eq!(dst, vec![30, 20, 10, 255]);
    }

    #[test]
    fn a_frame_is_opaque_and_scaled() {
        let title = "Servo".to_string();
        let loading = [true];
        let pixels = render(&View {
            width: 640,
            look: Look::Eclipse,
            titles: std::slice::from_ref(&title),
            loading: &loading,
            active: 0,
            location: "https://example.com",
            location_focused: true,
            location_cursor: 0,
            location_suggest: "",
            find_mode: false,
            scheme: SchemeHint::Https,
            gpu: true,
            can_back: false,
            can_forward: true,
            scale: 2,
        });
        assert_eq!(pixels.width, 1280);
        assert_eq!(pixels.height, HEIGHT * 2);
        assert_eq!(pixels.argb.len(), (1280 * HEIGHT * 2 * 4) as usize);
        assert_eq!(pixels.argb[3], 255);
    }

    #[test]
    fn every_toolbar_control_has_its_own_hit() {
        let lay = layout(900, 3);
        let point = |r: RectI| (r.x + r.w / 2, r.y + r.h / 2);
        let (x, y) = point(lay.back);
        assert_eq!(hit(900, 3, x, y), Some(Hit::Back));
        let (x, y) = point(lay.forward);
        assert_eq!(hit(900, 3, x, y), Some(Hit::Forward));
        let (x, y) = point(lay.reload);
        assert_eq!(hit(900, 3, x, y), Some(Hit::Reload));
        let (x, y) = point(lay.new_tab);
        assert_eq!(hit(900, 3, x, y), Some(Hit::NewTab));
        let (x, y) = point(lay.tabs[1]);
        assert_eq!(hit(900, 3, x, y), Some(Hit::Tab(1)));
        let (x, y) = point(lay.closes[2]);
        assert_eq!(hit(900, 3, x, y), Some(Hit::Close(2)));
    }

    #[test]
    fn many_tabs_stay_inside_the_strip_and_the_close_box_stays_on_its_tab() {
        let width = 640;
        let count = 24;
        let lay = layout(width, count);
        assert_eq!(lay.tabs.len(), count);
        assert!(lay.new_tab.x + lay.new_tab.w <= width);
        for (tab, close) in lay.tabs.iter().zip(&lay.closes) {
            assert!(tab.x >= 0);
            assert!(tab.x + tab.w <= lay.new_tab.x);
            assert!(close.x >= tab.x);
            assert!(close.x + close.w <= tab.x + tab.w);
            assert_eq!(
                hit(width, count, close.x, close.y),
                Some(Hit::Close(
                    lay.closes.iter().position(|c| c == close).unwrap()
                ))
            );
        }
    }

    #[test]
    fn a_window_shorter_than_the_chrome_still_has_a_page_pixel() {
        let (w, h) = content_px(800, 40, 1);
        assert_eq!(w, 800);
        assert_eq!(h, 1);
        let (w, h) = content_px(0, 0, 0);
        assert_eq!(w, 1);
        assert_eq!(h, 1);
        let (w, h) = content_px(800, 600, 2);
        assert_eq!(w, 1600);
        assert_eq!(h, ((600 - HEIGHT) * 2) as u32);
    }

    #[test]
    fn blit_swaps_channels_and_stops_at_the_buffer() {
        let src = [1u8, 2, 3, 255, 4, 5, 6, 255];
        let mut dst = [0u8; 8];
        blit_rgba(&src, 2, 1, &mut dst, 1, 8, 0);
        assert_eq!(&dst[..4], &[3, 2, 1, 255]);
        assert_eq!(&dst[4..], &[0, 0, 0, 0]);

        let mut tall = [9u8; 16];
        blit_rgba(&src, 1, 2, &mut tall, 1, 8, 1);
        assert_eq!(&tall[..8], &[9, 9, 9, 9, 9, 9, 9, 9]);
        assert_eq!(&tall[8..12], &[3, 2, 1, 255]);
        assert_eq!(&tall[12..], &[9, 9, 9, 9]);
    }

    #[test]
    fn every_look_paints_an_opaque_strip() {
        for look in [Look::Eclipse, Look::Kde, Look::Win11] {
            let title = "pestaña".to_string();
            let loading = [false];
            let pixels = render(&View {
                width: 320,
                look,
                titles: std::slice::from_ref(&title),
                loading: &loading,
                active: 0,
                location: "",
                location_focused: false,
                location_cursor: 0,
                location_suggest: "",
                find_mode: false,
                scheme: SchemeHint::Other,
                gpu: false,
                can_back: true,
                can_forward: false,
                scale: 1,
            });
            assert_eq!(pixels.width, 320);
            assert_eq!(pixels.height, HEIGHT);
            assert!(pixels.argb.chunks_exact(4).all(|px| px[3] == 255));
        }
    }

    #[test]
    fn the_scheme_badge_sits_before_the_location_field() {
        let lay = layout(800, 1);
        assert!(lay.scheme.x < lay.location.x);
        assert!(lay.scheme.x > lay.reload.x);
        assert_eq!(lay.scheme.w, SCHEME_W);
        assert!(lay.location.x + lay.location.w < lay.gpu.x);
        assert_eq!(lay.gpu.w, GPU_W);
    }
}
