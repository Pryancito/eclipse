//! Vector text for the Lunar clients: DejaVu Sans Mono's outlines as
//! tiny-skia paths, fitted to a fixed character cell.
//!
//! Every Lunar client used to draw text from a bitmap face (embedded-graphics'
//! 9x15 and 6x10, lunarbg's hand-made 5x7), so every curve and diagonal was a
//! staircase. This crate turns the TrueType outlines into paths that the
//! client fills anti-aliased, while keeping each client's CELL: a character
//! still advances exactly the cell's width, so every layout computed in cells
//! stays where it was and only the ink inside each cell changes.
//!
//! A glyph is centred in its cell and sits on the cell's baseline, both
//! rounded to whole pixels, so stems land on the pixel grid the same way every
//! time. No hinting is applied: the outlines are drawn as designed.
//!
//! The face is read from disk once, from the directories the image installs
//! DejaVu into (or `LUNAR_FONT_DIR`). When it is not there, or
//! `LUNAR_TEXT=bitmap` is set, [`available`] is false and the client keeps its
//! bitmap face; a character the face has no outline for is
//! [`Glyph::Missing`], and the client draws that one cell with its bitmap
//! face.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tiny_skia::{Path, PathBuilder, Transform};
use ttf_parser::{Face, OutlineBuilder};

pub use tiny_skia;

/// Which cut of the face.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Weight {
    Regular,
    Bold,
}

/// The character cell glyphs are fitted to, in pixels.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cell {
    /// Cell width: the advance of every character.
    pub width: f32,
    /// Pixel size of the em. DejaVu Sans Mono advances 0.602 em, so an em
    /// of `width / 0.602` fills the cell edge to edge; a smaller one leaves
    /// air between letters.
    pub em: f32,
    /// Baseline, in pixels below the top of the cell.
    pub baseline: f32,
}

impl Cell {
    fn key(self) -> (u32, u32, u32) {
        (self.width.to_bits(), self.em.to_bits(), self.baseline.to_bits())
    }
}

/// Where the image puts DejaVu: Alpine's `font-dejavu`, then Debian's
/// `fonts-dejavu-core`.
const FONT_DIRS: &[&str] = &[
    "/usr/share/fonts/dejavu",
    "/usr/share/fonts/truetype/dejavu",
    "/usr/share/fonts/TTF",
];

struct Faces {
    regular: Face<'static>,
    bold: Face<'static>,
}

fn load_face(dir: &str, file: &str) -> Option<Face<'static>> {
    let bytes = std::fs::read(format!("{dir}/{file}")).ok()?;
    // Read once per process and kept for its whole life: the face borrows the
    // bytes, and the clients never unload their font.
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    Face::parse(bytes, 0).ok()
}

fn load() -> Option<Faces> {
    if std::env::var("LUNAR_TEXT").is_ok_and(|v| v == "bitmap") {
        return None;
    }
    let dirs: Vec<String> = match std::env::var("LUNAR_FONT_DIR") {
        Ok(d) if !d.is_empty() => vec![d],
        _ => FONT_DIRS.iter().map(|d| d.to_string()).collect(),
    };
    for dir in &dirs {
        if let Some(regular) = load_face(dir, "DejaVuSansMono.ttf") {
            // No bold cut on disk is a regular-only face, not no face.
            let bold =
                load_face(dir, "DejaVuSansMono-Bold.ttf").unwrap_or_else(|| regular.clone());
            return Some(Faces { regular, bold });
        }
    }
    None
}

fn faces() -> Option<&'static Faces> {
    static FACES: OnceLock<Option<Faces>> = OnceLock::new();
    FACES.get_or_init(load).as_ref()
}

/// Whether the vector face was found (and not switched off).
pub fn available() -> bool {
    faces().is_some()
}

/// Turns an outline (font units, y up) into a tiny-skia path in pixels
/// (y down), with the glyph origin at (`dx`, 0).
struct Builder {
    pb: PathBuilder,
    scale: f32,
    dx: f32,
}

impl Builder {
    fn p(&self, x: f32, y: f32) -> (f32, f32) {
        (self.dx + x * self.scale, -y * self.scale)
    }
}

impl OutlineBuilder for Builder {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.p(x, y);
        self.pb.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.p(x, y);
        self.pb.line_to(x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (x1, y1) = self.p(x1, y1);
        let (x, y) = self.p(x, y);
        self.pb.quad_to(x1, y1, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (x1, y1) = self.p(x1, y1);
        let (x2, y2) = self.p(x2, y2);
        let (x, y) = self.p(x, y);
        self.pb.cubic_to(x1, y1, x2, y2, x, y);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

/// What the cache holds for one character.
#[derive(Clone, Debug)]
pub enum Glyph {
    /// Nothing to draw (a space, or a glyph with no contours).
    Blank,
    /// The outline, in pixels, relative to the cell's top-left corner.
    Ink(Path),
    /// The face has no such character (or there is no face): draw it with
    /// the bitmap face.
    Missing,
}

/// Build the path of `ch` fitted to `cell`, relative to the cell's top-left.
fn build(face: &Face<'static>, ch: char, cell: Cell) -> Glyph {
    let Some(id) = face.glyph_index(ch) else {
        return Glyph::Missing;
    };
    let scale = cell.em / face.units_per_em() as f32;
    let advance = face.glyph_hor_advance(id).unwrap_or(0) as f32 * scale;
    // Centred in the cell, snapped to a whole pixel so every instance of a
    // letter lands on the grid the same way.
    let dx = ((cell.width - advance) / 2.0).round();
    let mut b = Builder {
        pb: PathBuilder::new(),
        scale,
        dx,
    };
    if face.outline_glyph(id, &mut b).is_none() {
        return Glyph::Blank;
    }
    match b.pb.finish() {
        Some(path) => match path.transform(Transform::from_translate(0.0, cell.baseline.round())) {
            Some(p) => Glyph::Ink(p),
            None => Glyph::Blank,
        },
        None => Glyph::Blank,
    }
}

type Key = (Weight, (u32, u32, u32), char);

/// The glyph for `ch`, built once per process and then reused: a panel that
/// repaints every second says the same few dozen characters each time.
pub fn glyph(ch: char, weight: Weight, cell: Cell) -> Glyph {
    static CACHE: OnceLock<Mutex<HashMap<Key, Glyph>>> = OnceLock::new();
    let Some(f) = faces() else {
        return Glyph::Missing;
    };
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    // A panic elsewhere while holding the lock leaves a cache that is still a
    // valid map: keep using it.
    let mut cache = cache.lock().unwrap_or_else(|p| p.into_inner());
    cache
        .entry((weight, cell.key(), ch))
        .or_insert_with(|| {
            let face = match weight {
                Weight::Regular => &f.regular,
                Weight::Bold => &f.bold,
            };
            build(face, ch, cell)
        })
        .clone()
}

/// Rasterise `ch` into an 8-bit coverage mask the size of its cell, for a
/// client that blends pixels itself instead of drawing through tiny-skia
/// (lunarbg). `None` when there is no face or no such character.
pub fn coverage(ch: char, weight: Weight, cell: Cell, height: u32) -> Option<(u32, u32, Vec<u8>)> {
    let Glyph::Ink(path) = glyph(ch, weight, cell) else {
        return None;
    };
    let w = cell.width.ceil().max(1.0) as u32;
    let mut mask = tiny_skia::Mask::new(w, height.max(1))?;
    mask.fill_path(&path, tiny_skia::FillRule::Winding, true, Transform::identity());
    Some((w, height.max(1), mask.data().to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: Cell = Cell {
        width: 9.0,
        em: 14.0,
        baseline: 12.0,
    };

    #[test]
    fn every_glyph_stays_inside_its_cell_horizontally() {
        if !available() {
            return;
        }
        for ch in "AMWmw@#0Ññ·▸".chars() {
            if let Glyph::Ink(p) = glyph(ch, Weight::Bold, TEXT) {
                let b = p.bounds();
                assert!(b.left() >= -1.0, "{ch:?} spills left: {b:?}");
                assert!(b.right() <= TEXT.width + 1.0, "{ch:?} spills right: {b:?}");
            }
        }
    }

    #[test]
    fn capitals_stand_on_the_baseline() {
        if !available() {
            return;
        }
        let Glyph::Ink(p) = glyph('H', Weight::Regular, TEXT) else {
            panic!("no H");
        };
        let b = p.bounds();
        assert!((b.bottom() - TEXT.baseline).abs() < 0.01, "{b:?}");
        assert!(b.top() >= 0.0, "the capital pokes out of the cell: {b:?}");
        assert!((9.0..=11.5).contains(&b.height()), "{b:?}");
    }

    #[test]
    fn a_space_draws_nothing_and_an_unknown_character_falls_back() {
        if !available() {
            return;
        }
        assert!(matches!(glyph(' ', Weight::Regular, TEXT), Glyph::Blank));
        assert!(matches!(glyph('\u{F8FF}', Weight::Regular, TEXT), Glyph::Missing));
    }

    #[test]
    fn the_coverage_mask_has_ink_and_full_strength_stems() {
        if !available() {
            return;
        }
        let (w, h, m) = coverage('I', Weight::Regular, TEXT, 15).expect("mask");
        assert_eq!((w, h), (9, 15));
        assert!(m.iter().any(|&a| a == 255), "no fully covered pixel");
        assert!(m.iter().any(|&a| a > 0 && a < 255), "no anti-aliased edge");
    }
}
