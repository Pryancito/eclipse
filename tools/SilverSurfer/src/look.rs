//! Which look the desktop is wearing. The same three names `eclipse-look`
//! writes to `/etc/eclipse/look`, so the chrome matches lunarbar.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    Win11,
    Kde,
    Eclipse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub panel: Rgb,
    pub field: Rgb,
    pub text: Rgb,
    pub dim: Rgb,
    pub accent: Rgb,
    pub border: Rgb,
}

impl Look {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "win11" | "windows" | "windows11" => Some(Look::Win11),
            "kde" | "breeze" => Some(Look::Kde),
            "eclipse" => Some(Look::Eclipse),
            _ => None,
        }
    }

    /// `$ECLIPSE_LOOK` wins, then the first `look=` line of the file, then
    /// Eclipse. An unrecognised environment value falls through to the file.
    pub fn resolve(env: Option<&str>, file_text: Option<&str>) -> Self {
        if let Some(look) = env.and_then(Self::from_name) {
            return look;
        }
        file_text
            .and_then(look_from_text)
            .unwrap_or(Look::Eclipse)
    }

    pub fn current() -> Self {
        let env = std::env::var("ECLIPSE_LOOK").ok();
        let file = std::fs::read_to_string("/etc/eclipse/look").ok();
        Self::resolve(env.as_deref(), file.as_deref())
    }

    /// Path `eclipse-look` writes; used to detect mid-session changes.
    pub const FILE: &'static str = "/etc/eclipse/look";

    pub fn palette(self) -> Palette {
        match self {
            Look::Kde => Palette {
                panel: rgb(0x2a, 0x2e, 0x32),
                field: rgb(0x1b, 0x1e, 0x20),
                text: rgb(0xfc, 0xfc, 0xfc),
                dim: rgb(0x7f, 0x8c, 0x8d),
                accent: rgb(0x3d, 0xae, 0xe9),
                border: rgb(0x4d, 0x4d, 0x4d),
            },
            Look::Eclipse => Palette {
                panel: rgb(0x0b, 0x12, 0x20),
                field: rgb(0x12, 0x21, 0x38),
                text: rgb(0xff, 0xff, 0xff),
                dim: rgb(0x6d, 0x7f, 0xa3),
                accent: rgb(0x6e, 0xa8, 0xff),
                border: rgb(0x27, 0x5a, 0x9e),
            },
            Look::Win11 => Palette {
                panel: rgb(0x2b, 0x2b, 0x2b),
                field: rgb(0x1c, 0x1c, 0x1c),
                text: rgb(0xff, 0xff, 0xff),
                dim: rgb(0xc5, 0xc5, 0xc5),
                accent: rgb(0x00, 0x78, 0xd4),
                border: rgb(0x3d, 0x3d, 0x3d),
            },
        }
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

/// First `look=` line. A typo on that line is not a later, valid line: the
/// file said something, and it did not name a look.
fn look_from_text(text: &str) -> Option<Look> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(value) = line.strip_prefix("look=") {
            return Look::from_name(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_case_insensitive() {
        assert_eq!(Look::from_name("KDE"), Some(Look::Kde));
        assert_eq!(Look::from_name(" win11 "), Some(Look::Win11));
        assert_eq!(Look::from_name("breeze"), Some(Look::Kde));
        assert_eq!(Look::from_name("nope"), None);
    }

    #[test]
    fn environment_beats_the_file_and_a_typo_falls_through() {
        let file = "look=kde\n";
        assert_eq!(Look::resolve(Some("eclipse"), Some(file)), Look::Eclipse);
        assert_eq!(Look::resolve(Some("nope"), Some(file)), Look::Kde);
        assert_eq!(Look::resolve(None, Some("# c\nlook=win11\n")), Look::Win11);
        assert_eq!(Look::resolve(None, None), Look::Eclipse);
    }

    #[test]
    fn a_typo_on_the_first_line_does_not_fall_through() {
        assert_eq!(
            Look::resolve(None, Some("look=nope\nlook=kde\n")),
            Look::Eclipse
        );
    }

    #[test]
    fn the_three_palettes_are_distinct() {
        let eclipse = Look::Eclipse.palette();
        let kde = Look::Kde.palette();
        let win = Look::Win11.palette();
        assert_ne!(eclipse.accent, kde.accent);
        assert_ne!(eclipse.accent, win.accent);
        assert_ne!(kde.accent, win.accent);
        assert_eq!(eclipse.panel, rgb(0x0b, 0x12, 0x20));
    }
}
