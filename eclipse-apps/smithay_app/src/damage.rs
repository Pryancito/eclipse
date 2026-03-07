//! Damage tracking: rectángulos, merge, overlaps.
//! Patrones inspirados en cosmic-comp.

use embedded_graphics::primitives::Rectangle;
use embedded_graphics::geometry::{Point, Size};

pub fn union_rects(a: Rectangle, b: Rectangle) -> Rectangle {
    let x1 = a.top_left.x.min(b.top_left.x);
    let y1 = a.top_left.y.min(b.top_left.y);
    let x2 = (a.top_left.x + a.size.width as i32).max(b.top_left.x + b.size.width as i32);
    let y2 = (a.top_left.y + a.size.height as i32).max(b.top_left.y + b.size.height as i32);
    Rectangle::new(Point::new(x1, y1), Size::new((x2 - x1) as u32, (y2 - y1) as u32))
}

/// Rects se solapan (AABB)
pub fn rects_overlap(a: Rectangle, b: Rectangle) -> bool {
    let ax2 = a.top_left.x + a.size.width as i32;
    let ay2 = a.top_left.y + a.size.height as i32;
    let bx2 = b.top_left.x + b.size.width as i32;
    let by2 = b.top_left.y + b.size.height as i32;
    a.top_left.x < bx2 && ax2 > b.top_left.x && a.top_left.y < by2 && ay2 > b.top_left.y
}

/// true si `outer` contiene completamente a `inner`
pub fn rect_contains(outer: Rectangle, inner: Rectangle) -> bool {
    let ox1 = outer.top_left.x;
    let oy1 = outer.top_left.y;
    let ox2 = ox1 + outer.size.width as i32;
    let oy2 = oy1 + outer.size.height as i32;
    let ix1 = inner.top_left.x;
    let iy1 = inner.top_left.y;
    let ix2 = ix1 + inner.size.width as i32;
    let iy2 = iy1 + inner.size.height as i32;
    ox1 <= ix1 && oy1 <= iy1 && ox2 >= ix2 && oy2 >= iy2
}

/// Fusiona rectángulos solapados; reduce blits redundantes
pub fn merge_overlapping_rects<const N: usize>(rects: &mut heapless::Vec<Rectangle, N>) {
    let mut changed = true;
    while changed && rects.len() > 1 {
        changed = false;
        'outer: for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                if rects_overlap(rects[i], rects[j]) {
                    rects[i] = union_rects(rects[i], rects[j]);
                    rects.swap_remove(j);
                    changed = true;
                    break 'outer;
                }
            }
        }
    }
}

/// Devuelve la intersección entre dos rectángulos.
pub fn rect_intersection(a: Rectangle, b: Rectangle) -> Option<Rectangle> {
    let x1 = a.top_left.x.max(b.top_left.x);
    let y1 = a.top_left.y.max(b.top_left.y);
    let x2 = (a.top_left.x + a.size.width as i32).min(b.top_left.x + b.size.width as i32);
    let y2 = (a.top_left.y + a.size.height as i32).min(b.top_left.y + b.size.height as i32);

    if x2 > x1 && y2 > y1 {
        Some(Rectangle::new(Point::new(x1, y1), Size::new((x2 - x1) as u32, (y2 - y1) as u32)))
    } else {
        None
    }
}

/// Resta el rectángulo `occluder` del rectángulo `base`, devolviendo una lista de hasta 4
/// rectángulos resultantes que cubren el área visible.
pub fn subtract_rect(base: Rectangle, occluder: Rectangle) -> heapless::Vec<Rectangle, 4> {
    let mut result = heapless::Vec::new();
    let inter = match rect_intersection(base, occluder) {
        Some(i) => i,
        None => {
            let _ = result.push(base);
            return result;
        }
    };

    // Dividir 'base' en trozos que quedan fuera de 'inter' (que es la parte oculta)
    let bx1 = base.top_left.x;
    let by1 = base.top_left.y;
    let bx2 = bx1 + base.size.width as i32;
    let by2 = by1 + base.size.height as i32;

    let ix1 = inter.top_left.x;
    let iy1 = inter.top_left.y;
    let ix2 = ix1 + inter.size.width as i32;
    let iy2 = iy1 + inter.size.height as i32;

    // Top piece
    if iy1 > by1 {
        let _ = result.push(Rectangle::new(Point::new(bx1, by1), Size::new(base.size.width, (iy1 - by1) as u32)));
    }
    // Bottom piece
    if iy2 < by2 {
        let _ = result.push(Rectangle::new(Point::new(bx1, iy2), Size::new(base.size.width, (by2 - iy2) as u32)));
    }
    // Left piece
    if ix1 > bx1 {
        let _ = result.push(Rectangle::new(Point::new(bx1, iy1), Size::new((ix1 - bx1) as u32, inter.size.height)));
    }
    // Right piece
    if ix2 < bx2 {
        let _ = result.push(Rectangle::new(Point::new(ix2, iy1), Size::new((bx2 - ix2) as u32, inter.size.height)));
    }

    result
}
