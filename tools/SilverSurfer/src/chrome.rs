//! The toolbar drawn into a wl_shm buffer. The page never paints this strip:
//! it is a subsurface of its own, so Servo's GL window starts below it.

use embedded_graphics::mono_font::ascii::FONT_6X10;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::text::Text;
use tiny_skia::{Color, Paint, Pixmap, Rect, Transform};

use crate::location::SchemeHint;
use crate::look::{Look, Rgb};

pub const TAB_H: i32 = 28;
pub const TOOL_H: i32 = 36;
pub const HEIGHT: i32 = TAB_H + TOOL_H;

const PAD: i32 = 8;
const BTN: i32 = 24;
const GAP: i32 = 4;
const SCHEME_W: i32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RectI {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl RectI {
    fn contains(self, x: i32, y: i32) -> bool {
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
            w: (width - PAD - loc_x).max(1),
            h: BTN,
        },
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
    pub scheme: SchemeHint,
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
    let logical = render_logical(view);
    let argb = if scale == 1 {
        logical
    } else {
        nearest(&logical, view.width.max(1), HEIGHT, scale)
    };
    Pixels {
        argb,
        width: view.width.max(1) * scale,
        height: HEIGHT * scale,
    }
}

fn render_logical(view: &View<'_>) -> Vec<u8> {
    let width = view.width.max(1) as u32;
    let height = HEIGHT as u32;
    let mut pixmap = Pixmap::new(width, height).expect("chrome pixmap");
    let pal = view.look.palette();
    pixmap.fill(color(pal.panel));
    let lay = layout(view.width.max(1), view.titles.len().max(1));
    let titles: &[String] = if view.titles.is_empty() {
        &[]
    } else {
        view.titles
    };
    for (i, rect) in lay.tabs.iter().enumerate() {
        let on = i == view.active;
        fill(
            &mut pixmap,
            *rect,
            if on { pal.field } else { pal.panel },
        );
        if on {
            fill(
                &mut pixmap,
                RectI {
                    x: rect.x,
                    y: rect.y + rect.h - 2,
                    w: rect.w,
                    h: 2,
                },
                pal.accent,
            );
        }
    }
    fill(&mut pixmap, lay.new_tab, pal.field);
    fill(&mut pixmap, lay.back, pal.field);
    fill(&mut pixmap, lay.forward, pal.field);
    fill(&mut pixmap, lay.reload, pal.field);
    fill(&mut pixmap, lay.scheme, pal.field);
    fill(&mut pixmap, lay.location, pal.field);
    if view.location_focused {
        stroke(&mut pixmap, lay.location, pal.accent);
    }
    if view.scheme == SchemeHint::Https {
        fill(
            &mut pixmap,
            RectI {
                x: lay.scheme.x,
                y: lay.scheme.y + lay.scheme.h - 2,
                w: lay.scheme.w,
                h: 2,
            },
            pal.accent,
        );
    }

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
        label(&mut target, rect.x + 6, rect.y + 14, &clip(&title, room), text);
        if rect.w >= 20 {
            label(&mut target, rect.x + rect.w - 12, rect.y + 14, "x", dim);
        }
    }
    label(&mut target, lay.new_tab.x + 8, lay.new_tab.y + 14, "+", text);
    label(
        &mut target,
        lay.back.x + 8,
        lay.back.y + 16,
        "<",
        if view.can_back { text } else { dim },
    );
    label(
        &mut target,
        lay.forward.x + 8,
        lay.forward.y + 16,
        ">",
        if view.can_forward { text } else { dim },
    );
    label(&mut target, lay.reload.x + 8, lay.reload.y + 16, "R", text);
    let scheme_color = if view.scheme == SchemeHint::Https {
        pal.accent
    } else {
        dim
    };
    label(
        &mut target,
        lay.scheme.x + 5,
        lay.scheme.y + 16,
        view.scheme.badge(),
        scheme_color,
    );
    let loc = if view.location.is_empty() {
        "Buscar o escribir una dirección"
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
    label(
        &mut target,
        lay.location.x + 8,
        lay.location.y + 16,
        &shown,
        loc_color,
    );
    argb
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

fn fill(pixmap: &mut Pixmap, rect: RectI, rgb: Rgb) {
    let Some(r) = Rect::from_xywh(rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32)
    else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color_rgba8(rgb.r, rgb.g, rgb.b, 255);
    pixmap.fill_rect(r, &paint, Transform::identity(), None);
}

fn stroke(pixmap: &mut Pixmap, rect: RectI, rgb: Rgb) {
    fill(
        pixmap,
        RectI {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: 1,
        },
        rgb,
    );
    fill(
        pixmap,
        RectI {
            x: rect.x,
            y: rect.y + rect.h - 1,
            w: rect.w,
            h: 1,
        },
        rgb,
    );
    fill(
        pixmap,
        RectI {
            x: rect.x,
            y: rect.y,
            w: 1,
            h: rect.h,
        },
        rgb,
    );
    fill(
        pixmap,
        RectI {
            x: rect.x + rect.w - 1,
            y: rect.y,
            w: 1,
            h: rect.h,
        },
        rgb,
    );
}

fn label(target: &mut ArgbTarget<'_>, x: i32, y: i32, text: &str, rgb: Rgb) {
    let style = MonoTextStyle::new(&FONT_6X10, Rgb888::new(rgb.r, rgb.g, rgb.b));
    let _ = Text::new(text, Point::new(x, y), style).draw(target);
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

fn nearest(src: &[u8], width: i32, height: i32, scale: i32) -> Vec<u8> {
    let scale = scale.max(1);
    let dw = width * scale;
    let dh = height * scale;
    let mut out = vec![0u8; (dw * dh * 4) as usize];
    for y in 0..dh {
        for x in 0..dw {
            let sx = x / scale;
            let sy = y / scale;
            let si = ((sy * width + sx) * 4) as usize;
            let di = ((y * dw + x) * 4) as usize;
            out[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    out
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
            scheme: SchemeHint::Https,
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
                scheme: SchemeHint::Other,
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
    }
}
