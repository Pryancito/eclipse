//! Vector text: DejaVu Sans Mono's outlines, filled anti-aliased by tiny-skia.
//!
//! The bitmap faces (`FONT_9X15`, its bold, `FONT_10X20`) gave every Lunar
//! client stair-stepped letters: a glyph is either on or off at each pixel, so
//! every diagonal and every curve is a staircase. This module draws the same
//! strings from the TrueType outlines instead, with real coverage at the
//! edges, while keeping the bitmap faces' CELLS: a character still advances
//! exactly `GLYPH_W` (or `BIG_W`) pixels and its cell is still `GLYPH_H` (or
//! `BIG_H`) tall. Every layout in the clients is computed in those cells, so
//! none of them moves by a pixel; only the ink inside each cell changes.
//!
//! Each glyph is centred in its cell and sits on a baseline fixed per cell
//! size, both rounded to whole pixels so stems land on the pixel grid the same
//! way every time (no hinting is applied: the outlines are drawn as designed,
//! and the snapping keeps a string from shimmering as it moves).
//!
//! The face is read from disk once, from the directories the image installs
//! DejaVu into. When it is not there, or `LUNAR_TEXT=bitmap` is set, nothing
//! here draws and [`crate::draw::Canvas`] falls back to the bitmap faces; a
//! character the face has no outline for falls back the same way, one cell at
//! a time.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tiny_skia::{Path, PathBuilder};
use ttf_parser::{Face, OutlineBuilder};

/// Which cut of the face.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Weight {
    Regular,
    Bold,
}

/// Which cell the glyphs are fitted to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cell {
    /// `GLYPH_W` x `GLYPH_H`: the text of every client.
    Text,
    /// `BIG_W` x `BIG_H`: the tecno clock readout.
    Big,
}

impl Cell {
    /// Pixel size of the em, chosen so the face's advance fits the cell's
    /// width and its capitals stand as tall as the bitmap face's did.
    pub fn em_px(self) -> f32 {
        match self {
            Cell::Text => 14.0,
            Cell::Big => 17.0,
        }
    }

    /// Baseline, in pixels below the top of the cell.
    pub fn baseline(self) -> f32 {
        match self {
            Cell::Text => 12.0,
            Cell::Big => 16.0,
        }
    }

    pub fn width(self) -> f32 {
        match self {
            Cell::Text => crate::draw::GLYPH_W as f32,
            Cell::Big => crate::draw::BIG_W as f32,
        }
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
            let bold = load_face(dir, "DejaVuSansMono-Bold.ttf")
                .unwrap_or_else(|| regular.clone());
            return Some(Faces { regular, bold });
        }
    }
    None
}

fn faces() -> Option<&'static Faces> {
    static FACES: OnceLock<Option<Faces>> = OnceLock::new();
    FACES.get_or_init(load).as_ref()
}

#[cfg(test)]
thread_local! {
    /// This library's own tests draw with the bitmap faces unless one asks
    /// for the vector face, so their exact-pixel counts do not depend on
    /// whether the machine has DejaVu. (The binaries' suites link this crate
    /// without `cfg(test)`, so they draw with whatever the machine has and
    /// must hold under both engines; `LUNAR_TEXT=bitmap` runs them on the
    /// bitmap one.)
    static TEST_VECTOR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with vector text switched on for this thread (tests only).
#[cfg(test)]
pub fn with_vector<R>(f: impl FnOnce() -> R) -> R {
    TEST_VECTOR.with(|t| t.set(true));
    let r = f();
    TEST_VECTOR.with(|t| t.set(false));
    r
}

/// Whether vector text is available at all.
pub fn available() -> bool {
    #[cfg(test)]
    if !TEST_VECTOR.with(|t| t.get()) {
        return false;
    }
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
#[derive(Clone)]
pub enum Glyph {
    /// Nothing to draw (a space, or a glyph with no contours).
    Blank,
    /// The outline, in pixels, relative to the cell's top-left corner.
    Ink(Path),
    /// The face has no such character: draw it with the bitmap face.
    Missing,
}

/// Build the path of `ch` fitted to `cell`, relative to the cell's top-left.
fn build(face: &Face<'static>, ch: char, cell: Cell) -> Glyph {
    let Some(id) = face.glyph_index(ch) else {
        return Glyph::Missing;
    };
    let scale = cell.em_px() / face.units_per_em() as f32;
    let advance = face.glyph_hor_advance(id).unwrap_or(0) as f32 * scale;
    // Centred in the cell, snapped to a whole pixel so every instance of a
    // letter lands on the grid the same way.
    let dx = ((cell.width() - advance) / 2.0).round();
    let mut b = Builder {
        pb: PathBuilder::new(),
        scale,
        dx,
    };
    if face.outline_glyph(id, &mut b).is_none() {
        return Glyph::Blank;
    }
    let base = cell.baseline();
    match b.pb.finish() {
        Some(path) => match path.transform(tiny_skia::Transform::from_translate(0.0, base)) {
            Some(p) => Glyph::Ink(p),
            None => Glyph::Blank,
        },
        None => Glyph::Blank,
    }
}

type Key = (Weight, Cell, char);

/// The glyph for `ch`, built once per process and then reused: the panel
/// repaints every second and says the same few dozen characters each time.
pub fn glyph(ch: char, weight: Weight, cell: Cell) -> Glyph {
    static CACHE: OnceLock<Mutex<HashMap<Key, Glyph>>> = OnceLock::new();
    let Some(f) = faces() else {
        return Glyph::Missing;
    };
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = match cache.lock() {
        Ok(c) => c,
        // A panic elsewhere while holding the lock leaves a cache that is
        // still a valid map: keep using it.
        Err(p) => p.into_inner(),
    };
    cache
        .entry((weight, cell, ch))
        .or_insert_with(|| {
            let face = match weight {
                Weight::Regular => &f.regular,
                Weight::Bold => &f.bold,
            };
            build(face, ch, cell)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn have_font() -> bool {
        faces().is_some()
    }

    #[test]
    fn every_glyph_stays_inside_its_cell_horizontally() {
        if !have_font() {
            return;
        }
        for cell in [Cell::Text, Cell::Big] {
            for ch in "AMWmw@#0Ññ·▸".chars() {
                if let Glyph::Ink(p) = glyph(ch, Weight::Bold, cell) {
                    let b = p.bounds();
                    assert!(b.left() >= -1.0, "{ch:?} {cell:?} spills left: {b:?}");
                    assert!(
                        b.right() <= cell.width() + 1.0,
                        "{ch:?} {cell:?} spills right: {b:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn capitals_stand_on_the_baseline_and_fit_the_cell() {
        if !have_font() {
            return;
        }
        let Glyph::Ink(p) = glyph('H', Weight::Regular, Cell::Text) else {
            panic!("no H");
        };
        let b = p.bounds();
        assert!((b.bottom() - Cell::Text.baseline()).abs() < 0.01, "{b:?}");
        assert!(b.top() >= 0.0, "the capital pokes out of the cell: {b:?}");
        // As tall as the bitmap face's capitals, give or take a pixel.
        assert!((9.0..=11.5).contains(&b.height()), "{b:?}");
    }

    #[test]
    fn a_space_draws_nothing_and_an_unknown_character_falls_back() {
        if !have_font() {
            return;
        }
        assert!(matches!(glyph(' ', Weight::Regular, Cell::Text), Glyph::Blank));
        // A private-use code point no font of the image carries.
        assert!(matches!(
            glyph('\u{F8FF}', Weight::Regular, Cell::Text),
            Glyph::Missing
        ));
    }

    #[test]
    fn the_suites_default_to_the_bitmap_face() {
        assert!(!available());
        assert_eq!(with_vector(available), have_font());
        assert!(!available());
    }
}
