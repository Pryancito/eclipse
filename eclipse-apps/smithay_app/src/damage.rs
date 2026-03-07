//! Damage tracking: rectángulos, merge, overlaps.
//! Patrones inspirados en cosmic-comp.

use embedded_graphics::primitives::Rectangle;
use embedded_graphics::geometry::{Point, Size};

#[inline]
pub fn union_rects(a: &Rectangle, b: &Rectangle) -> Rectangle {
    let x1 = a.top_left.x.min(b.top_left.x);
    let y1 = a.top_left.y.min(b.top_left.y);
    let x2 = (a.top_left.x + a.size.width as i32).max(b.top_left.x + b.size.width as i32);
    let y2 = (a.top_left.y + a.size.height as i32).max(b.top_left.y + b.size.height as i32);
    Rectangle::new(Point::new(x1, y1), Size::new((x2 - x1) as u32, (y2 - y1) as u32))
}

/// Rects se solapan (AABB)
#[inline]
pub fn rects_overlap(a: &Rectangle, b: &Rectangle) -> bool {
    let ax2 = a.top_left.x + a.size.width as i32;
    let ay2 = a.top_left.y + a.size.height as i32;
    let bx2 = b.top_left.x + b.size.width as i32;
    let by2 = b.top_left.y + b.size.height as i32;
    a.top_left.x < bx2 && ax2 > b.top_left.x && a.top_left.y < by2 && ay2 > b.top_left.y
}

/// true si `outer` contiene completamente a `inner`
#[inline]
pub fn rect_contains(outer: &Rectangle, inner: &Rectangle) -> bool {
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

/// Fusiona rectángulos solapados; reduce blits redundantes.
/// CORREGIDO: Uso de `while` para manejar tamaños dinámicos al hacer `swap_remove`.
pub fn merge_overlapping_rects<const N: usize>(rects: &mut heapless::Vec<Rectangle, N>) {
    let mut changed = true;
    while changed && rects.len() > 1 {
        changed = false;
        let mut i = 0;
        
        'outer: while i < rects.len() {
            let mut j = i + 1;
            while j < rects.len() {
                if rects_overlap(&rects[i], &rects[j]) {
                    // Fusionar y actualizar el rectángulo actual
                    rects[i] = union_rects(&rects[i], &rects[j]);
                    // Eliminar el que acabamos de absorber
                    rects.swap_remove(j);
                    changed = true;
                    // Rompemos para reiniciar el análisis desde el principio
                    // con el nuevo arreglo modificado
                    break 'outer;
                }
                j += 1;
            }
            i += 1;
        }
    }
}

/// Devuelve la intersección entre dos rectángulos.
#[inline]
pub fn rect_intersection(a: &Rectangle, b: &Rectangle) -> Option<Rectangle> {
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

/// Resta el rectángulo `occluder` del rectángulo `base`, escribiendo el resultado en `out`.
/// Usa un buffer reutilizable para mantener el stack constante.
pub fn subtract_rect(base: &Rectangle, occluder: &Rectangle, out: &mut heapless::Vec<Rectangle, 4>) {
    out.clear();
    let inter = match rect_intersection(base, occluder) {
        Some(i) => i,
        None => {
            // Si no se tocan, el resultado es simplemente el rectángulo original
            let _ = out.push(*base);
            return;
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

    // Pieza Superior
    if iy1 > by1 {
        let _ = out.push(Rectangle::new(Point::new(bx1, by1), Size::new(base.size.width, (iy1 - by1) as u32)));
    }
    // Pieza Inferior
    if iy2 < by2 {
        let _ = out.push(Rectangle::new(Point::new(bx1, iy2), Size::new(base.size.width, (by2 - iy2) as u32)));
    }
    // Pieza Izquierda
    if ix1 > bx1 {
        let _ = out.push(Rectangle::new(Point::new(bx1, iy1), Size::new((ix1 - bx1) as u32, inter.size.height)));
    }
    // Pieza Derecha
    if ix2 < bx2 {
        let _ = out.push(Rectangle::new(Point::new(ix2, iy1), Size::new((bx2 - ix2) as u32, inter.size.height)));
    }
}