//! Vector text for this crate's clients, over [`lunartext`].
//!
//! The bitmap faces (`FONT_9X15`, its bold, `FONT_10X20`) gave every Lunar
//! client stair-stepped letters. [`crate::draw::Canvas`] now fills DejaVu Sans
//! Mono's outlines anti-aliased, fitted to the SAME cells: a character still
//! advances exactly `GLYPH_W` (or `BIG_W`) pixels in a `GLYPH_H` (or `BIG_H`)
//! cell, so every layout computed in cells stays where it was and only the
//! ink inside each cell changes. The face, the cache and the fallback rules
//! live in `tools/lunartext`, shared with lunarbg and SilverSurfer.

pub use lunartext::{Glyph, Weight};

/// Which of this crate's cells the glyphs are fitted to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cell {
    /// `GLYPH_W` x `GLYPH_H`: the text of every client.
    Text,
    /// `BIG_W` x `BIG_H`: the tecno clock readout.
    Big,
}

impl Cell {
    pub fn width(self) -> f32 {
        self.fit().width
    }

    /// The em is chosen so the face's advance fits the cell's width with a
    /// little air, and its capitals stand as tall as the bitmap face's.
    fn fit(self) -> lunartext::Cell {
        match self {
            Cell::Text => lunartext::Cell {
                width: crate::draw::GLYPH_W as f32,
                em: 14.0,
                baseline: 12.0,
            },
            Cell::Big => lunartext::Cell {
                width: crate::draw::BIG_W as f32,
                em: 17.0,
                baseline: 16.0,
            },
        }
    }
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
    lunartext::available()
}

/// The glyph for `ch` in one of this crate's cells.
pub fn glyph(ch: char, weight: Weight, cell: Cell) -> Glyph {
    lunartext::glyph(ch, weight, cell.fit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_stays_inside_its_cell_horizontally() {
        if !lunartext::available() {
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
    fn the_suites_default_to_the_bitmap_face() {
        assert!(!available());
        assert_eq!(with_vector(available), lunartext::available());
        assert!(!available());
    }
}
