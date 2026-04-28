//! Render del SSD (titlebar + bordes + botones) con `tiny-skia`.
//!
//! Smithay nos da el output framebuffer (`pixman`) y nosotros pintamos por
//! encima la decoración del lado servidor — exactamente como hace labwc con
//! cairo/pango pero usando tiny-skia (no_std-float friendly).
//!
//! El render se ejecuta cada frame en el callback de `output`. Para cada
//! `Window` del `Space` que tenga `decoration_mode = ServerSide`:
//!
//! 1. Pintar el borde (1px) — color `theme.border_active|inactive`.
//! 2. Pintar la titlebar de altura `theme.title_height` — `theme.title_bg_*`.
//! 3. Pintar el label (título de la ventana) centrado.
//! 4. Pintar los 3 botones (close/max/min) a la derecha.

use tiny_skia::{Color, Paint, PathBuilder, PixmapMut, Rect, Transform};

use crate::ssd::{button_rects, TitleButton};
use crate::theme::{Rgba, Theme};

fn rgba_to_color(c: Rgba) -> Color {
    Color::from_rgba8(c.0, c.1, c.2, c.3)
}

/// Pinta SSD para una ventana. `pixmap` es el output framebuffer
/// (acceso compartido con Smithay tras `Renderer::render` o vía pixman backend).
///
/// `(wx, wy, ww, wh)` son las coordenadas de la zona cliente. La titlebar se
/// dibuja por encima de `wy` (en `wy - title_h`).
pub fn draw_ssd(pix: &mut PixmapMut<'_>, theme: &Theme, active: bool, title: &str,
                wx: i32, wy: i32, ww: i32, wh: i32) {
    let bw = theme.border_width;
    let th = theme.title_height;

    // 1) Borde.
    let border = if active { theme.border_active } else { theme.border_inactive };
    fill(pix, wx - bw, wy - th - bw, ww + 2*bw, th + wh + 2*bw, border);

    // 2) Titlebar bg.
    let bg = if active { theme.title_bg_active } else { theme.title_bg_inactive };
    fill(pix, wx, wy - th, ww, th, bg);

    // 3) Label centrado — tiny-skia no incluye text shaping; pintamos un
    //    placeholder geométrico (línea de pixels) cuya longitud refleja el
    //    título. Para texto real, la integración es con `fontdue` o
    //    `cosmic-text` que el usuario quiera enchufar (ver TODO en README).
    let fg = if active { theme.title_fg_active } else { theme.title_fg_inactive };
    let label_w = (title.chars().count() as i32 * 7).min(ww - 100);
    if label_w > 0 {
        fill(pix, wx + 8, wy - th + th/2 - 1, label_w, 2, fg);
    }

    // 4) Botones (close / max / min) — usamos los rects calculados en `ssd.rs`.
    use crate::view::View;
    use crate::view::ViewKind;
    let view = View {
        kind: ViewKind::XdgToplevel, client_pid: 0, surface_id: 0,
        x: wx, y: wy - th, w: ww, h: th + wh,
        stored: (0,0,0,0), maximized: false, minimized: false, fullscreen: false,
        ssd: true, closing: false, workspace: 1,
        title: alloc::string::String::new(), app_id: alloc::string::String::new(), z: 0,
    };
    for (btn, bx, by, bw2, bh2) in button_rects(&view, theme) {
        let c = match btn {
            TitleButton::Close    => theme.btn_close,
            TitleButton::Maximize => theme.btn_max,
            TitleButton::Minimize => theme.btn_min,
        };
        // Botón redondeado — usando tiny-skia path API.
        if let Some(rect) = Rect::from_xywh(bx as f32, by as f32, bw2 as f32, bh2 as f32) {
            let mut paint = Paint::default();
            paint.set_color(rgba_to_color(c));
            paint.anti_alias = true;
            let radius = (bh2 as f32 / 2.0).min(theme.corner_radius as f32);
            let path = rounded_rect(rect, radius);
            pix.fill_path(&path, &paint, tiny_skia::FillRule::Winding,
                          Transform::identity(), None);
        }
    }
}

fn fill(pix: &mut PixmapMut<'_>, x: i32, y: i32, w: i32, h: i32, c: Rgba) {
    if w <= 0 || h <= 0 { return; }
    let rect = match Rect::from_xywh(x as f32, y as f32, w as f32, h as f32) {
        Some(r) => r, None => return,
    };
    let mut paint = Paint::default();
    paint.set_color(rgba_to_color(c));
    pix.fill_rect(rect, &paint, Transform::identity(), None);
}

fn rounded_rect(rect: Rect, radius: f32) -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    let r = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
    let (x, y, w, h) = (rect.x(), rect.y(), rect.width(), rect.height());
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
    pb.finish().unwrap()
}

extern crate alloc;
