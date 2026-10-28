//! The two window themes brought in from labwc's own repositories.
//!
//! Moebius pointed at `labwc/labwc-artwork` and `labwc/labwc-themes`. The
//! first one no longer holds window themes at all -- its last commit moved
//! the final one out ("Transfer Combine to labwc/labwc-themes"), leaving SVG
//! *button* sets, wallpapers, an icon font and the labwc logo -- so the
//! themes come from the second.
//!
//! Two of the twelve are installed here, and the choice is not a matter of
//! taste:
//!
//! * **`labAdwaita-dark`** is the one Eclipse was already half-wearing:
//!   `write_gtk_settings` sets `gtk-theme-name=Adwaita-dark` and
//!   `gtk-icon-theme-name=Adwaita`, so every GTK application in the session
//!   is Adwaita dark while the frame around it was not. Its buttons are SVG.
//! * **`labAdwaita-dark-xbm`** is the same theme with XBM buttons, and it is
//!   the one the `gnome` look actually selects. labwc only reads SVG buttons
//!   when it was built with librsvg (`-Dsvg=enabled`), and the labwc in the
//!   image comes from the rootfs's own package manager, not from xtask -- so
//!   whether SVG works is not knowable here, and the look that can fail is
//!   not the one to point a switch at. XBM buttons have been recoloured from
//!   `window.*.button.*.image.color` since openbox and need no librsvg at
//!   all. The SVG twin is installed beside it for anyone who knows their
//!   labwc has librsvg: `<name>labAdwaita-dark</name>` in `rc.xml`.
//!
//! What is deliberately NOT brought in: labwc's wallpapers and logos (they
//! are labwc's branding, and Eclipse renders its own wallpaper), the
//! `labwcmoon` icon font (572 KiB for glyphs lunarbar draws itself), the
//! `maccish-enhanced` button set (its files are named `close-active1.svg`,
//! which labwc does not look for), and the other ten themes -- including
//! `labArc-xbm`, which was in this list until the suite next door pointed
//! out that it paints the focused and the unfocused titlebar the same
//! `#2f343f` and draws no border at all, so the window with the keyboard is
//! told apart only by the tint of its close button. On a stack with no
//! shadows that is the grey-on-grey case Eclipse already paid for once in
//! `Breeze-Dark`.
//!
//! # Where the files land
//!
//! Upstream ships each theme as `<name>/labwc/`, the directory labwc has
//! read since 0.8; before that the only name was `openbox-3/`. The labwc in
//! the image is whatever the rootfs's package manager installed, so the
//! files go in `openbox-3/` and `labwc` is a relative symlink beside it. One
//! copy of the bytes, found under either name.
//!
//! # Licence
//!
//! CC BY-SA 4.0, upstream's own licence, copied verbatim into the rootfs next
//! to the themes as `/usr/share/themes/LICENSE.labwc-themes` together with
//! the attribution and the commit the files came from. The files themselves
//! are vendored byte-for-byte under `xtask/assets/labwc-themes/`: a themerc
//! retyped by hand is a different work, and share-alike is easier to honour
//! when nothing was touched.

use std::{fs, path::Path};

/// The upstream commit the vendored files were taken from.
const UPSTREAM_COMMIT: &str = "2c26fbd248ac24e88a12da4fbb90bdbada074cf8";

/// `include_bytes!` for every file of one vendored theme, as
/// `(name, bytes)` pairs. The names are spelled out rather than globbed
/// because `include_bytes!` needs a literal: a file added to the asset
/// directory and not named here is simply not installed, which the test
/// below is what catches.
macro_rules! theme {
    ($dir:literal, [$($f:literal),+ $(,)?]) => {
        Theme {
            name: $dir,
            files: &[$((
                $f,
                include_bytes!(concat!(
                    "../../../assets/labwc-themes/", $dir, "/labwc/", $f
                )) as &[u8],
            )),+],
        }
    };
}

struct Theme {
    /// Theme name, i.e. the directory under `/usr/share/themes` and the
    /// string that goes in `<theme><name>` in `rc.xml`.
    name: &'static str,
    files: &'static [(&'static str, &'static [u8])],
}

const THEMES: &[Theme] = &[
    theme!(
        "labAdwaita-dark",
        [
            "close-active.svg",
            "close-inactive.svg",
            "close_hover-active.svg",
            "close_hover-inactive.svg",
            "desk-active.svg",
            "desk-inactive.svg",
            "desk_hover-active.svg",
            "desk_hover-inactive.svg",
            "desk_toggled-active.svg",
            "desk_toggled-inactive.svg",
            "desk_toggled_hover-active.svg",
            "desk_toggled_hover-inactive.svg",
            "iconify-active.svg",
            "iconify-inactive.svg",
            "iconify_hover-active.svg",
            "iconify_hover-inactive.svg",
            "max-active.svg",
            "max-inactive.svg",
            "max_hover-active.svg",
            "max_hover-inactive.svg",
            "max_toggled-active.svg",
            "max_toggled-inactive.svg",
            "menu-active.svg",
            "menu-inactive.svg",
            "menu_hover-active.svg",
            "menu_hover-inactive.svg",
            "shade-active.svg",
            "shade-inactive.svg",
            "shade_hover-active.svg",
            "shade_hover-inactive.svg",
            "shade_toggled-active.svg",
            "shade_toggled-inactive.svg",
            "shade_toggled_hover-inactive.svg",
            "themerc",
        ]
    ),
    theme!(
        "labAdwaita-dark-xbm",
        [
            "close.xbm",
            "iconify.xbm",
            "max.xbm",
            "max_toggled.xbm",
            "menu.xbm",
            "shade-toggled.xbm",
            "shade.xbm",
            "themerc",
        ]
    ),
];

/// The names of the themes imported here, as they appear under
/// `/usr/share/themes` and in `<theme><name>` in `rc.xml`.
///
/// Attribution is per work, so the licence notice is built from this rather
/// than from a sentence that goes stale the day a theme is added or dropped.
/// The tests next door read it for the other reason: they hold Eclipse's OWN
/// themes to a single list of colour keys -- a slot one theme fills and
/// another leaves out is a look showing one of labwc's built-in colours --
/// and an imported theme cannot be held to it, because which slots to fill
/// is the upstream author's decision and editing a CC BY-SA work until it
/// matches our key list is the one thing vendoring it verbatim was meant to
/// avoid. What they are held to instead is everything that is not a matter
/// of taste: a themerc that paints an active titlebar, a focused window told
/// apart from an unfocused one, a palette of their own, and a `sed` that can
/// switch back out of them.
pub(super) fn names() -> impl Iterator<Item = &'static str> {
    THEMES.iter().map(|t| t.name)
}

/// Install both themes and the terminal palette that goes with them.
///
/// Called from [`super::install`] after `write_foot_config`, which is what
/// creates `~/.config/foot` and writes the three palettes that were already
/// there; the one added here sits beside them under the same name
/// `eclipse-look` copies from (`foot.<look>.ini`).
pub(super) fn install(rootfs: &Path) {
    let themes = rootfs.join("usr/share/themes");
    for theme in THEMES {
        // openbox-3 holds the bytes; `labwc` is the name a 0.8+ labwc looks
        // for first. Writing the directory twice would be the other way to
        // satisfy both, and 100 KiB heavier for nothing.
        let dir = themes.join(theme.name).join("openbox-3");
        let _ = fs::create_dir_all(&dir);
        for (name, bytes) in theme.files {
            fs::write(dir.join(name), bytes).unwrap();
        }
        let link = themes.join(theme.name).join("labwc");
        let _ = fs::remove_file(&link);
        let _ = std::os::unix::fs::symlink("openbox-3", &link);
    }
    let _ = fs::create_dir_all(&themes);
    let credited: Vec<&str> = names().collect();
    fs::write(
        themes.join("LICENSE.labwc-themes"),
        format!(
            "{} {} from https://github.com/labwc/labwc-themes\n\
             at commit {UPSTREAM_COMMIT}, installed unmodified.\n\
             \n\
             Copyright the labwc-themes contributors.\n\
             Licensed under the Creative Commons Attribution-ShareAlike 4.0\n\
             International licence (CC BY-SA 4.0):\n\
             https://creativecommons.org/licenses/by-sa/4.0/\n\
             \n\
             The full licence text ships with the Eclipse source tree at\n\
             xtask/assets/labwc-themes/LICENSE.CC-BY-SA-4.0.\n",
            credited.join(" and "),
            if credited.len() == 1 { "is" } else { "are" },
        ),
    )
    .unwrap();

    let foot = rootfs.join("root/.config/foot");
    let _ = fs::create_dir_all(&foot);
    fs::write(foot.join("foot.gnome.ini"), super::foot_ini(FOOT_ADWAITA)).unwrap();
}

/// GNOME Terminal's own dark palette, so the terminal in the `gnome` look
/// matches `labAdwaita-dark` rather than staying violet.
const FOOT_ADWAITA: &str = "\
background=1d1d1d\nforeground=ffffff\n\
regular0=241f31\nregular1=c01c28\nregular2=2ec27e\nregular3=f5c211\n\
regular4=1e78e4\nregular5=9841bb\nregular6=0ab9dc\nregular7=c0bfbc\n\
bright0=5e5c64\nbright1=f66151\nbright2=33d17a\nbright3=f8e45c\n\
bright4=3584e4\nbright5=c061cb\nbright6=4fd2fd\nbright7=ffffff\n";

#[cfg(test)]
mod tests;
