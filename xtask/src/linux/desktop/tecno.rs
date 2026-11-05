//! The `tecno` look: Eclipse dressed as an instrument panel.
//!
//! Its own file rather than four more constants in `desktop.rs` because a
//! look is a window theme AND a terminal palette AND a line in four places of
//! `eclipse-look`, and the two halves that are colour belong next to each
//! other — the frame and the terminal inside it are what the user sees at the
//! same time, and they drifting apart is exactly what happened to the
//! original: `Eclipse-Dark` is still violet (`#191430`) while the panel went
//! blue, so today the frame and the bar do not match.
//!
//! What makes it "tecno" is not the hue. The panel draws the same layout as
//! `eclipse` with square plates framed by a 1px hairline instead of rounded,
//! gold focus brackets on its popups and launcher, its module labels
//! in capitals and its meters segmented (see `Look::Tecno` in
//! `tools/lunarbar`). This file supplies the pieces the panel cannot draw:
//! the window frame, which belongs to labwc, foot's palette, and the GTK
//! stylesheet (which Firefox's "system" theme reads its colours from too).
//!
//! Colours, shared with `PAL_TECNO` in `tools/lunarbar/src/main.rs`:
//!
//! | role | hex |
//! |---|---|
//! | ground | `#060b14` |
//! | plate / titlebar | `#0a1423` |
//! | border, separators | `#1d3550` |
//! | accent (focus, selection) | `#38e1ff` |
//! | text | `#e6f1ff` |
//! | dim text | `#8aa4c2` |

use std::{fs, path::Path};

/// The window theme, the terminal palette and the GTK stylesheet this look
/// needs.
pub(super) fn install(rootfs: &Path) {
    write_theme(rootfs);
    write_foot(rootfs);
    write_gtk(rootfs);
}

/// The widgets both GTK versions share: square, hairline-framed, cyan focus.
const GTK_COMMON: &str = include_str!("../../../assets/tecno-gtk/common.css");
/// GTK 3's named colours: what Firefox paints its toolbar and tabs from.
const GTK3_COLOURS: &str = include_str!("../../../assets/tecno-gtk/colours3.css");
/// libadwaita's named colours, which repaint its apps on their own.
const GTK4_COLOURS: &str = include_str!("../../../assets/tecno-gtk/colours4.css");

/// `gtk.tecno.css` for GTK 3 and 4, which `eclipse-look` copies over
/// `gtk.css` (and removes for a look without one). Adwaita-dark stays the
/// theme underneath: this is a user stylesheet on top of it, so a look that
/// drops it falls back to stock Adwaita-dark and not to a broken theme.
fn write_gtk(rootfs: &Path) {
    for (ver, colours) in [("gtk-3.0", GTK3_COLOURS), ("gtk-4.0", GTK4_COLOURS)] {
        let dir = rootfs.join("root/.config").join(ver);
        let _ = fs::create_dir_all(&dir);
        let css = format!("{colours}{GTK_COMMON}");
        fs::write(dir.join("gtk.tecno.css"), &css).unwrap();
        // tecno is the default look, so GTK starts in it too.
        fs::write(dir.join("gtk.css"), &css).unwrap();
    }
}

/// `Tecno-Dark`: an openbox-3 themerc naming exactly the keys Eclipse's other
/// themes name, so `eclipse-look` can come back out of it and so the suite
/// next door can hold all of them to one contract.
///
/// The focused window is told from an unfocused one by TWO things and not one
/// — the accent border and a brighter titlebar — because this stack draws
/// shadows only on labwc >= 0.8 and the image's labwc comes from the rootfs's
/// package manager, so whether there is a shadow is not knowable here.
fn write_theme(rootfs: &Path) {
    let dir = rootfs.join("usr/share/themes/Tecno-Dark/openbox-3");
    let _ = fs::create_dir_all(&dir);
    fs::write(
        dir.join("themerc"),
        b"# Eclipse OS - tecno: instrument-panel colours for labwc/openbox.\n\
          border.width: 1\n\
          padding.width: 8\n\
          padding.height: 5\n\
          \n\
          window.active.border.color: #38e1ff\n\
          window.inactive.border.color: #1d3550\n\
          window.active.title.bg.color: #0a1423\n\
          window.inactive.title.bg.color: #060b14\n\
          window.active.label.text.color: #e6f1ff\n\
          window.inactive.label.text.color: #8aa4c2\n\
          window.active.button.unpressed.image.color: #8aa4c2\n\
          window.active.button.pressed.image.color: #f2c14e\n\
          window.active.button.hover.image.color: #38e1ff\n\
          window.inactive.button.unpressed.image.color: #3a5878\n\
          # Close is the one button in the warn colour, as in the mockup.\n\
          window.active.button.close.unpressed.image.color: #ff8a5c\n\
          window.active.button.close.hover.image.color: #ffab88\n\
          window.inactive.button.close.unpressed.image.color: #3a5878\n\
          # Titles left-aligned, like an instrument readout.\n\
          window.label.text.justify: Left\n\
          \n\
          menu.title.bg.color: #0a1423\n\
          menu.title.text.color: #38e1ff\n\
          menu.items.bg.color: #080e1a\n\
          menu.items.text.color: #e6f1ff\n\
          menu.items.disabled.text.color: #3a5878\n\
          menu.items.active.bg.color: #0c2a3a\n\
          menu.items.active.text.color: #38e1ff\n\
          menu.separator.color: #1d3550\n\
          menu.separator.padding.height: 4\n\
          \n\
          osd.bg.color: #060b14\n\
          osd.border.color: #1d3550\n\
          osd.border.width: 1\n\
          osd.label.text.color: #e6f1ff\n\
          # Alt+Tab (labwc >= 0.9 names; older builds ignore them): a square\n\
          # plate with a hairline edge, rows on the 4 px grid, the selected\n\
          # row boxed by a 1 px line, and the window it will raise outlined\n\
          # in the gold of the focus brackets.\n\
          osd.window-switcher.style-classic.width: 640\n\
          osd.window-switcher.style-classic.padding: 8\n\
          osd.window-switcher.style-classic.item.padding.x: 12\n\
          osd.window-switcher.style-classic.item.padding.y: 4\n\
          osd.window-switcher.style-classic.item.active.border.width: 1\n\
          osd.window-switcher.style-classic.item.icon.size: 20\n\
          osd.window-switcher.preview.border.width: 2\n\
          osd.window-switcher.preview.border.color: #f2c14e\n\
          # The desktop switcher's boxes, as the bar's desktop chips.\n\
          osd.workspace-switcher.boxes.width: 16\n\
          osd.workspace-switcher.boxes.height: 16\n\
          osd.workspace-switcher.boxes.border.width: 1\n\
          \n\
          # Drop shadows (labwc >= 0.8; ignored by older builds). Kept tight\n\
          # and dark: this look leans on a hairline border, and a soft wide\n\
          # shadow around it reads as a halo rather than as depth.\n\
          window.active.shadow.size: 16\n\
          window.active.shadow.color: #000000 50\n\
          window.inactive.shadow.size: 8\n\
          window.inactive.shadow.color: #000000 30\n",
    )
    .unwrap();
    // The buttons: square frames with the glyph inside, as XBM so they need
    // no librsvg (see `upstream.rs`) and take their colour from the keys
    // above. 16x16, drawn 1:1 by labwc and centred in its 26px button.
    for (name, bits) in BUTTONS {
        fs::write(dir.join(name), bits).unwrap();
    }
}

/// The window buttons of the "Orbital HUD" mockup: a 1px square with the
/// glyph inside it. Generated once and vendored under `xtask/assets`.
const BUTTONS: &[(&str, &[u8])] = &[
    (
        "close.xbm",
        include_bytes!("../../../assets/tecno-buttons/close.xbm"),
    ),
    (
        "iconify.xbm",
        include_bytes!("../../../assets/tecno-buttons/iconify.xbm"),
    ),
    (
        "max.xbm",
        include_bytes!("../../../assets/tecno-buttons/max.xbm"),
    ),
    (
        "max_toggled.xbm",
        include_bytes!("../../../assets/tecno-buttons/max_toggled.xbm"),
    ),
];

/// `foot.tecno.ini`, which `eclipse-look` copies over `foot.ini` when the
/// look is switched. A terminal left in the previous look's palette is the
/// loudest thing on the screen, because it is the biggest flat colour in the
/// session.
fn write_foot(rootfs: &Path) {
    let dir = rootfs.join("root/.config/foot");
    let _ = fs::create_dir_all(&dir);
    fs::write(dir.join("foot.tecno.ini"), super::foot_ini(FOOT_TECNO)).unwrap();
    // tecno is the default look, so the terminal starts in it too
    // (`write_foot_config` ran first and wrote eclipse's).
    fs::write(dir.join("foot.ini"), super::foot_ini(FOOT_TECNO)).unwrap();
}

/// The terminal palette. Cyan and mint carry the "live" end of the ramp
/// (`regular4`/`regular6`) because that is what the panel's accent is; red is
/// kept as the warm orange `#ff8a5c` the panel warns in, so "bad" means the
/// same colour in the bar and in the terminal.
const FOOT_TECNO: &str = "\
background=060b14\nforeground=e6f1ff\n\
regular0=0a1423\nregular1=ff8a5c\nregular2=7af0c8\nregular3=f2c14e\n\
regular4=38e1ff\nregular5=9ab8ff\nregular6=5fe0d8\nregular7=bcd8de\n\
bright0=1d3550\nbright1=ffa98a\nbright2=a8ffe0\nbright3=f7d685\n\
bright4=7ae8f5\nbright5=bcd0ff\nbright6=9af0ea\nbright7=f5f9ff\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn rootfs(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-tecno-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        install(&dir);
        dir
    }

    /// The frame and the panel have to name the SAME colours, and nothing in
    /// the build can check that for us: `PAL_TECNO` lives in
    /// `tools/lunarbar`, which is not a member of this workspace. What went
    /// wrong once already is exactly this drift -- `Eclipse-Dark` is still
    /// violet while the panel it frames went blue -- so the hexes are pinned
    /// here, where a change to either side has to come past them.
    #[test]
    fn the_window_frame_is_painted_in_the_panels_own_colours() {
        let dir = rootfs("frame");
        let rc = fs::read_to_string(dir.join("usr/share/themes/Tecno-Dark/openbox-3/themerc"))
            .expect("Tecno-Dark themerc");
        for (role, hex) in [
            ("the accent on the focused border", "#38e1ff"),
            ("the titlebar plate", "#0a1423"),
            ("the ground behind an unfocused title", "#060b14"),
            ("the text", "#e6f1ff"),
            ("the dim text", "#8aa4c2"),
            ("the separator", "#1d3550"),
        ] {
            assert!(
                rc.contains(hex),
                "{role} ({hex}) is not in the themerc:\n{rc}"
            );
        }
        // The focused window is told apart by TWO things, not one: this stack
        // only draws shadows on labwc >= 0.8 and the image's labwc comes from
        // the rootfs's package manager, so a single cue can be the only cue.
        assert!(rc.contains("window.active.border.color: #38e1ff"));
        assert!(rc.contains("window.inactive.border.color: #1d3550"));
        assert!(rc.contains("window.active.title.bg.color: #0a1423"));
        assert!(rc.contains("window.inactive.title.bg.color: #060b14"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// labwc reads `<button>.xbm` beside the themerc. Each one has to be a
    /// real 16x16 XBM with its frame drawn: a missing or empty file falls
    /// back to labwc's built-in glyph and the frames silently disappear.
    #[test]
    fn the_window_buttons_are_framed_xbm_glyphs() {
        let root = rootfs("buttons");
        write_theme(&root);
        let dir = root.join("usr/share/themes/Tecno-Dark/openbox-3");
        for name in ["close", "iconify", "max", "max_toggled"] {
            let xbm = fs::read_to_string(dir.join(format!("{name}.xbm"))).unwrap();
            assert!(xbm.contains(&format!("#define {name}_width 16")), "{name}");
            assert!(xbm.contains(&format!("#define {name}_height 16")), "{name}");
            // The first two bytes are the top row of the frame: all set.
            let first = xbm.split('{').nth(1).unwrap();
            assert!(
                first.trim_start().starts_with("0xff, 0xff"),
                "{name} has no frame"
            );
        }
        let rc = fs::read_to_string(dir.join("themerc")).unwrap();
        assert!(rc.contains("window.active.button.close.unpressed.image.color: #ff8a5c"));
        assert!(rc.contains("window.label.text.justify: Left"));
        let _ = fs::remove_dir_all(&root);
    }

    /// A colour written `#10 28 33` is three tokens to openbox and not one
    /// hex, so it silently falls back to black. The first draft of this very
    /// file had that typo in `menu.items.active.bg.color`, which would have
    /// landed as "the selected menu row went invisible".
    #[test]
    fn every_colour_in_the_theme_is_one_hex_token() {
        let dir = rootfs("hex");
        let rc = fs::read_to_string(dir.join("usr/share/themes/Tecno-Dark/openbox-3/themerc"))
            .expect("Tecno-Dark themerc");
        for line in rc.lines().filter(|l| !l.trim_start().starts_with('#')) {
            let Some((key, val)) = line.split_once(':') else {
                continue;
            };
            if !key.trim().ends_with(".color") {
                continue;
            }
            // `#rrggbb`, optionally followed by one opacity, which is what
            // the shadow colours carry.
            let mut parts = val.split_whitespace();
            let hex = parts.next().unwrap_or("");
            assert!(
                hex.len() == 7
                    && hex.starts_with('#')
                    && hex[1..].chars().all(|c| c.is_ascii_hexdigit()),
                "{key} is not one `#rrggbb`: {val:?}"
            );
            if let Some(extra) = parts.next() {
                assert!(
                    extra.parse::<u8>().map(|n| n <= 100).unwrap_or(false)
                        && parts.next().is_none(),
                    "{key} has something after its colour that is not an opacity: {val:?}"
                );
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The palette has to reach `foot.tecno.ini` under the name
    /// `eclipse-look` copies from, and carry foot's own header: the script
    /// does `cp -f foot.$look.ini foot.ini`, so a file named anything else is
    /// a switch that silently leaves the terminal in the previous look.
    #[test]
    fn the_terminal_palette_lands_where_the_switch_looks_for_it() {
        let dir = rootfs("foot");
        let ini = fs::read_to_string(dir.join("root/.config/foot/foot.tecno.ini"))
            .expect("foot.tecno.ini");
        assert!(
            ini.contains("background=060b14"),
            "not the tecno ground:\n{ini}"
        );
        assert!(
            ini.contains("foreground=e6f1ff"),
            "not the tecno text:\n{ini}"
        );
        assert!(
            ini.contains("regular4=38e1ff"),
            "the accent is not in the ramp:\n{ini}"
        );
        // Through `foot_ini`, so it inherits the single render worker and the
        // real monospace family the rest of the palettes get.
        assert!(
            ini.contains("\nworkers=1\n"),
            "built without foot_ini:\n{ini}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The stylesheet has to land under the name `eclipse-look` copies from,
    /// for both GTK versions, and the active `gtk.css` has to be the same
    /// file: tecno is the default look.
    #[test]
    fn the_gtk_stylesheet_lands_where_the_switch_looks_for_it() {
        let dir = rootfs("gtk");
        for ver in ["gtk-3.0", "gtk-4.0"] {
            let cfg = dir.join("root/.config").join(ver);
            let css = fs::read_to_string(cfg.join("gtk.tecno.css")).expect("gtk.tecno.css");
            let active = fs::read_to_string(cfg.join("gtk.css")).expect("gtk.css");
            assert_eq!(css, active, "{ver}: the default look is not the active one");
            for hex in ["#060b14", "#0a1423", "#1d3550", "#38e1ff", "#e6f1ff"] {
                assert!(css.contains(hex), "{ver}: {hex} missing");
            }
            assert!(
                css.contains("border-radius: 0"),
                "{ver}: corners not square"
            );
        }
        // Firefox reads GTK 3's exported names, libadwaita GTK 4's.
        let g3 = fs::read_to_string(dir.join("root/.config/gtk-3.0/gtk.css")).unwrap();
        assert!(g3.contains("@define-color theme_selected_bg_color #38e1ff;"));
        let g4 = fs::read_to_string(dir.join("root/.config/gtk-4.0/gtk.css")).unwrap();
        assert!(g4.contains("@define-color accent_bg_color #38e1ff;"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Every colour in the stylesheet is one of the palette's, so the GTK
    /// apps cannot drift from the panel the way `Eclipse-Dark` did.
    #[test]
    fn the_gtk_stylesheet_uses_only_the_palette() {
        let palette = [
            "#04070d", "#060b14", "#0a1423", "#10233a", "#1d3550", "#38e1ff", "#e6f1ff", "#8aa4c2",
            "#f2c14e", "#ff8a5c", "#6ee7a8",
        ];
        for css in [GTK_COMMON, GTK3_COLOURS, GTK4_COLOURS] {
            let mut rest = css;
            while let Some(i) = rest.find('#') {
                let hex: String = rest[i..].chars().take(7).collect();
                if hex[1..].chars().all(|c| c.is_ascii_hexdigit()) && hex.len() == 7 {
                    assert!(
                        palette.contains(&hex.as_str()),
                        "{hex} is not in the palette"
                    );
                }
                rest = &rest[i + 1..];
            }
        }
    }
}
