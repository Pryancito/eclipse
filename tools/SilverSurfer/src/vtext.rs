//! Vector labels for the browser chrome, over the shared `lunartext` crate.
//!
//! The chrome's labels were `FONT_6X10` stamped in `scale`-sized blocks, so on
//! a HiDPI output every letter was a staircase of 2x2 or 3x3 squares. These
//! are DejaVu Sans Mono's outlines in the same 6-px cells, rasterised at the
//! device scale with real coverage at the edges, so the chrome's layout (in
//! 6-px characters) does not move and only the letters change.

/// Draw `text` with its first cell at logical `x` and its baseline at logical
/// `y`, `scale` device pixels per logical one, into a premultiplied BGRA
/// buffer. `false` (nothing drawn) when the vector face is missing, switched
/// off, or lacks one of the characters: the caller then draws the bitmap face.
#[allow(clippy::too_many_arguments)]
pub(crate) fn label(
    data: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    text: &str,
    rgb: (u8, u8, u8),
    scale: i32,
) -> bool {
    // The suites count the bitmap face's pixels: they keep it.
    if cfg!(test) || !lunartext::available() {
        return false;
    }
    let s = scale.max(1) as f32;
    // FONT_6X10's capitals are 7 px on a baseline 8 px down its 10-px cell;
    // DejaVu's are 0.729 em, so a 9.5-px em matches them.
    let cell = lunartext::Cell {
        width: 6.0 * s,
        em: 9.5 * s,
        baseline: 8.0 * s,
    };
    let cell_h = (11.0 * s).ceil() as u32;
    let mut glyphs = Vec::new();
    for ch in text.chars() {
        if ch == ' ' {
            glyphs.push(None);
            continue;
        }
        match lunartext::coverage(ch, lunartext::Weight::Regular, cell, cell_h) {
            Some(g) => glyphs.push(Some(g)),
            // One character it cannot draw: the whole label stays bitmap, so
            // a label is never two faces at once.
            None => return false,
        }
    }
    let top = ((y as f32 - 8.0) * s).round() as i32;
    for (i, g) in glyphs.iter().enumerate() {
        let Some((gw, gh, mask)) = g else { continue };
        let left = ((x as f32 + 6.0 * i as f32) * s).round() as i32;
        for row in 0..*gh as i32 {
            for col in 0..*gw as i32 {
                let cov = mask[(row * *gw as i32 + col) as usize] as u32;
                let (px, py) = (left + col, top + row);
                if cov == 0 || px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
                    continue;
                }
                let k = ((py as u32 * width + px as u32) * 4) as usize;
                let Some(d) = data.get_mut(k..k + 4) else { continue };
                // Premultiplied "over" with an opaque source of coverage `cov`.
                let inv = 255 - cov;
                let mix = |src: u8, dst: u8| ((src as u32 * cov + dst as u32 * inv + 127) / 255) as u8;
                d[0] = mix(rgb.2, d[0]);
                d[1] = mix(rgb.1, d[1]);
                d[2] = mix(rgb.0, d[2]);
                d[3] = mix(255, d[3]);
            }
        }
    }
    true
}
