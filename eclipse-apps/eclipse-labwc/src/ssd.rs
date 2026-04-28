//! Server-Side Decorations — titlebar + 3 botones (close / max / min).
//!
//! Replicación del look-and-feel de labwc 0.8: barra de 28 px, label centrado,
//! botones a la derecha. Pintado real en `render.rs` usando `tiny-skia` /
//! `embedded-graphics` sobre el framebuffer Eclipse.

use crate::theme::Theme;
use crate::view::View;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// Nada — el cursor está sobre el client area.
    Client,
    /// El cursor está sobre la titlebar (pero no un botón).
    Title,
    /// El cursor está sobre el borde para resize.
    Border(BorderEdge),
    Button(TitleButton),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BorderEdge { N, S, E, W, NE, NW, SE, SW }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleButton { Close, Maximize, Minimize }

/// Layout fijo de los botones (de derecha a izquierda).
pub fn button_rects(view: &View, theme: &Theme) -> [(TitleButton, i32, i32, i32, i32); 3] {
    let bs = (theme.title_height - 8).max(16);     // tamaño del botón
    let pad = 4;
    let y = view.y + (theme.title_height - bs) / 2;
    let mut x = view.x + view.w - pad - bs;
    let close = (TitleButton::Close, x, y, bs, bs); x -= bs + pad;
    let max   = (TitleButton::Maximize, x, y, bs, bs); x -= bs + pad;
    let min   = (TitleButton::Minimize, x, y, bs, bs);
    [close, max, min]
}

/// Hit-testing: dado (cx,cy) en pantalla, ¿qué parte del SSD pulsó el usuario?
pub fn hit_test(view: &View, theme: &Theme, cx: i32, cy: i32) -> Hit {
    let bw = theme.border_width;
    let th = theme.title_height;
    let (x, y, w, h) = (view.x, view.y, view.w, view.h);

    // Bordes (resize) — sólo si la ventana no está maximizada.
    if !view.maximized {
        let on_n = cy >= y - bw && cy < y;
        let on_s = cy >= y + h && cy < y + h + bw;
        let on_w = cx >= x - bw && cx < x;
        let on_e = cx >= x + w && cx < x + w + bw;
        match (on_n, on_s, on_w, on_e) {
            (true, _, true, _)  => return Hit::Border(BorderEdge::NW),
            (true, _, _, true)  => return Hit::Border(BorderEdge::NE),
            (_, true, true, _)  => return Hit::Border(BorderEdge::SW),
            (_, true, _, true)  => return Hit::Border(BorderEdge::SE),
            (true, _, _, _)     => return Hit::Border(BorderEdge::N),
            (_, true, _, _)     => return Hit::Border(BorderEdge::S),
            (_, _, true, _)     => return Hit::Border(BorderEdge::W),
            (_, _, _, true)     => return Hit::Border(BorderEdge::E),
            _ => {}
        }
    }

    // Titlebar
    if cx >= x && cx < x + w && cy >= y && cy < y + th {
        for (btn, bx, by, bw2, bh2) in button_rects(view, theme) {
            if cx >= bx && cy >= by && cx < bx + bw2 && cy < by + bh2 {
                return Hit::Button(btn);
            }
        }
        return Hit::Title;
    }

    Hit::Client
}
