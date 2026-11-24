//! The accent colour of the `tecno` look, chosen in the control panel
//! (`LUNAR_ACCENT` in `/etc/eclipse/entorno`, read through
//! `lunartext::entorno`).
//!
//! `tecno` signals with one cyan, `#38e1ff`: the launcher, lit meters, the
//! focused chip, the clients' `Ink::cyan`. Each client keeps its palettes as
//! constants; [`tenida`] hands back the `tecno` one with that cyan swapped
//! for the chosen accent, made once per process. The gold of the focus
//! brackets is not the accent and never changes.

use std::sync::OnceLock;

use crate::draw::Rgb;

/// `tecno`'s own signal cyan, the default accent.
pub const CIAN: Rgb = (0x38, 0xe1, 0xff);

/// The accents the panel offers, by the word `LUNAR_ACCENT` holds. Each is
/// light enough for the dark `on_accent` text the palettes put on it.
pub const ACENTOS: [(&str, Rgb); 4] = [
    ("cian", CIAN),
    ("verde", (0x4e, 0xe6, 0xa0)),
    ("violeta", (0xa7, 0x8b, 0xfa)),
    ("rojo", (0xff, 0x5d, 0x6c)),
];

/// The accent a word names; an unknown word is the default.
pub fn de(word: Option<&str>) -> Rgb {
    word.and_then(|w| ACENTOS.iter().find(|(k, _)| *k == w.trim()))
        .map(|(_, c)| *c)
        .unwrap_or(CIAN)
}

/// The accent chosen for this process (read once).
pub fn elegido() -> Rgb {
    static A: OnceLock<Rgb> = OnceLock::new();
    *A.get_or_init(|| de(lunartext::entorno::var("LUNAR_ACCENT").as_deref()))
}

/// `c` in the chosen accent when it is `tecno`'s cyan, else `c`.
pub fn tinta(c: Rgb) -> Rgb {
    tinta_con(c, elegido())
}

/// [`tinta`] with the accent handed in, for tests.
pub fn tinta_con(c: Rgb, acento: Rgb) -> Rgb {
    if c == CIAN {
        acento
    } else {
        c
    }
}

/// `base` itself with the default accent, else a copy of it changed by
/// `tine`, made once in `cell`.
pub fn tenida<T: Clone + Send + Sync>(
    cell: &'static OnceLock<T>,
    base: &'static T,
    tine: impl FnOnce(&mut T),
) -> &'static T {
    if elegido() == CIAN {
        return base;
    }
    cell.get_or_init(|| {
        let mut p = base.clone();
        tine(&mut p);
        p
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_names_its_accent_and_anything_else_is_cyan() {
        assert_eq!(de(Some("verde")), (0x4e, 0xe6, 0xa0));
        assert_eq!(de(Some(" rojo ")), (0xff, 0x5d, 0x6c));
        assert_eq!(de(Some("ambar")), CIAN);
        assert_eq!(de(None), CIAN);
    }

    #[test]
    fn only_the_cyan_takes_the_accent() {
        let v = de(Some("violeta"));
        assert_eq!(tinta_con(CIAN, v), v);
        assert_eq!(
            tinta_con((0xf2, 0xc1, 0x4e), v),
            (0xf2, 0xc1, 0x4e),
            "the gold stays"
        );
    }

    #[test]
    fn every_accent_is_light_enough_for_dark_text() {
        for (w, (r, g, b)) in ACENTOS {
            let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
            assert!(luma > 110.0, "{w}: {luma}");
        }
    }
}
