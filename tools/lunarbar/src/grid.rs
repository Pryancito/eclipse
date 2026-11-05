//! The layout grid every Lunar surface is measured on: 4 px.
//!
//! Each client used to pick its own measures (margins of 8, 10, 14 and 16
//! px; headers of 34; rows of 26 and 30), so the launcher, the clipboard, the
//! file chooser and the panel's popups were each a couple of pixels off from
//! one another and from themselves. Now every margin, gap, row, header,
//! footer and box is a whole number of [`UNIT`]s, so edges line up across
//! surfaces and the vertical rhythm is the same everywhere.
//!
//! The rule is checked, not trusted: the test below reads every source file
//! of this crate and fails on any pixel measure (`*_H`, `*_W`, `PAD`, `GAP`,
//! ...) that is not a multiple of [`UNIT`]. The font cells (`GLYPH_*`,
//! `BIG_*`) are the face's own metrics and are exempt.

/// One grid step, in pixels.
pub const UNIT: i32 = 4;

/// `n` grid steps, in pixels.
pub const fn g(n: i32) -> i32 {
    n * UNIT
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A constant's name says it is a pixel measure.
    fn is_measure(name: &str) -> bool {
        const EXEMPT: &[&str] = &["GLYPH_H", "GLYPH_W", "BIG_H", "BIG_W"];
        if EXEMPT.contains(&name) {
            return false;
        }
        ["_H", "_W", "PAD", "GAP", "ICON", "CLOSE", "SHADOW", "_X", "_Y"]
            .iter()
            .any(|end| name.ends_with(end))
            || name == "PW"
            || name == "W"
    }

    fn sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                sources(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    #[test]
    fn every_pixel_measure_is_on_the_grid() {
        let mut files = Vec::new();
        sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let mut off = Vec::new();
        let mut seen = 0;
        for f in &files {
            let text = std::fs::read_to_string(f).unwrap();
            for (n, line) in text.lines().enumerate() {
                let l = line.trim_start();
                let l = l
                    .strip_prefix("pub(crate) ")
                    .or_else(|| l.strip_prefix("pub "))
                    .unwrap_or(l);
                let Some(rest) = l.strip_prefix("const ") else {
                    continue;
                };
                let Some((name, rest)) = rest.split_once(": i32 = ") else {
                    continue;
                };
                let Ok(v) = rest.trim_end_matches(';').trim().parse::<i32>() else {
                    continue;
                };
                if is_measure(name) {
                    seen += 1;
                    if v % UNIT != 0 {
                        off.push(format!("{}:{} {name} = {v}", f.display(), n + 1));
                    }
                }
            }
        }
        assert!(seen > 40, "the scan found only {seen} measures: is it reading the sources?");
        assert!(off.is_empty(), "off the {UNIT}-px grid:\n{}", off.join("\n"));
    }

    #[test]
    fn steps_are_whole_units() {
        assert_eq!(g(3), 12);
        assert_eq!(g(8) % UNIT, 0);
    }
}
