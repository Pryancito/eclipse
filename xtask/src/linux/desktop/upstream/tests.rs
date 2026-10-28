//! What the vendored themes are, read back out of the asset directory and out
//! of a rootfs they were installed into.
//!
//! Three things here cannot be seen by looking at the desktop, which is the
//! only reason they are tested at all: the asset directory and the
//! `include_bytes!` list drifting apart, a button file labwc will never look
//! for, and `labArc-xbm` quietly growing an SVG -- which would turn the
//! librsvg-free fallback into a second theme that needs librsvg.

use super::*;
use std::collections::BTreeSet;

/// Where the vendored files live, as the macro spells it.
fn asset_dir(theme: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/labwc-themes")
        .join(theme)
        .join("labwc")
}

fn on_disk(theme: &str) -> BTreeSet<String> {
    fs::read_dir(asset_dir(theme))
        .unwrap_or_else(|e| panic!("{}: {e}", asset_dir(theme).display()))
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

fn listed(theme: &Theme) -> BTreeSet<String> {
    theme.files.iter().map(|(n, _)| n.to_string()).collect()
}

/// `include_bytes!` needs a literal name, so the file list is typed out by
/// hand. A file dropped into the asset directory and not added to the list
/// is vendored, committed, reviewed -- and never installed.
#[test]
fn every_vendored_file_is_in_the_include_list() {
    for theme in THEMES {
        assert_eq!(
            on_disk(theme.name),
            listed(theme),
            "{}: the asset directory and the include list disagree",
            theme.name
        );
    }
}

/// A theme without a themerc is a directory of pictures: labwc finds no
/// colours in it and falls back to its built-in defaults, silently.
#[test]
fn every_theme_carries_a_themerc_with_colours_in_it() {
    for theme in THEMES {
        let (_, rc) = theme
            .files
            .iter()
            .find(|(n, _)| *n == "themerc")
            .unwrap_or_else(|| panic!("{} has no themerc", theme.name));
        let rc = std::str::from_utf8(rc).unwrap();
        assert!(
            rc.contains("window.active.title.bg.color"),
            "{}: the themerc paints no active titlebar",
            theme.name
        );
    }
}

/// The names labwc builds a button file name out of: one action, optionally
/// `_toggled` and/or `_hover`, then `-active` or `-inactive`. Anything else
/// is a file labwc never opens -- which is exactly how
/// `labwc-artwork`'s `maccish-enhanced` set is broken (`close-active1.svg`).
/// XBM buttons carry no state suffix at all: labwc recolours one bitmap per
/// action from `window.*.button.*.image.color`.
#[test]
fn every_button_file_is_a_name_labwc_looks_for() {
    const ACTIONS: &[&str] = &[
        "close",
        "iconify",
        "max",
        "menu",
        "shade",
        "desk",
        "window_menu",
    ];
    for theme in THEMES {
        for (name, _) in theme.files {
            if *name == "themerc" {
                continue;
            }
            let (stem, ext) = name
                .rsplit_once('.')
                .unwrap_or_else(|| panic!("{}: {name} has no extension", theme.name));
            assert!(
                ext == "svg" || ext == "xbm" || ext == "png",
                "{}: labwc reads svg, png and xbm buttons, not {ext} ({name})",
                theme.name
            );
            let mut rest = stem;
            if ext != "xbm" {
                rest = rest
                    .strip_suffix("-active")
                    .or_else(|| rest.strip_suffix("-inactive"))
                    .unwrap_or_else(|| {
                        panic!(
                            "{}: {name} ends in neither -active nor -inactive, \
                             so labwc never looks for it",
                            theme.name
                        )
                    });
            }
            // Upstream spells one Adwaita file `shade-toggled` with a dash
            // where every other file uses an underscore; both are stripped
            // here because the point of the test is the state suffix, not
            // upstream's typo.
            for state in ["_hover", "-hover", "_toggled", "-toggled"] {
                rest = rest.strip_suffix(state).unwrap_or(rest);
            }
            for state in ["_hover", "-hover"] {
                rest = rest.strip_suffix(state).unwrap_or(rest);
            }
            assert!(
                ACTIONS.contains(&rest),
                "{}: {name} names the button {rest:?}, which is not one of \
                 {ACTIONS:?} -- labwc will never open it",
                theme.name
            );
        }
    }
}

/// `labAdwaita-dark-xbm` is the theme `eclipse-look gnome` selects, and the
/// only reason to prefer it over its better-looking SVG twin is that XBM
/// buttons need no librsvg -- which the labwc in the rootfs may or may not
/// have been built with. An SVG in here would make the look that cannot fail
/// depend on the very thing it was chosen to avoid.
#[test]
fn the_xbm_theme_has_no_svg_in_it() {
    let xbm = THEMES
        .iter()
        .find(|t| t.name == "labAdwaita-dark-xbm")
        .expect("the librsvg-free theme is gone");
    for (name, _) in xbm.files {
        assert!(
            !name.ends_with(".svg"),
            "labAdwaita-dark-xbm carries {name}: it is no longer a theme \
             that works without librsvg"
        );
    }
}

/// CC BY-SA 4.0 is attribution *and* share-alike: the rootfs has to say where
/// the files came from, not just that they are licensed.
#[test]
fn the_installed_licence_names_the_source_and_the_themes() {
    let root = std::env::temp_dir().join(format!("eclipse-upstream-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    install(&root);
    let notice = fs::read_to_string(root.join("usr/share/themes/LICENSE.labwc-themes")).unwrap();
    assert!(notice.contains("labwc/labwc-themes"), "{notice}");
    assert!(notice.contains(UPSTREAM_COMMIT), "{notice}");
    assert!(notice.contains("CC BY-SA 4.0"), "{notice}");
    for theme in THEMES {
        assert!(
            notice.contains(theme.name),
            "{} is installed and the licence notice does not mention it",
            theme.name
        );
    }
    let _ = fs::remove_dir_all(&root);
}

/// Both names a labwc of any vintage might reach for resolve to the same
/// files: `openbox-3` since openbox, `labwc` since labwc 0.8.
#[test]
fn each_theme_is_reachable_under_both_directory_names() {
    let root = std::env::temp_dir().join(format!("eclipse-upstream-dirs-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    install(&root);
    for theme in THEMES {
        let dir = root.join("usr/share/themes").join(theme.name);
        for sub in ["openbox-3", "labwc"] {
            let rc = dir.join(sub).join("themerc");
            assert!(
                rc.is_file(),
                "{} is not readable as {}",
                theme.name,
                rc.display()
            );
        }
    }
    // Installing twice must be a no-op, not a failure: the rootfs build runs
    // incrementally and the `labwc` symlink is already there the second time.
    install(&root);
    let _ = fs::remove_dir_all(&root);
}

/// The palette `eclipse-look gnome` copies over `foot.ini`. A missing one is
/// silent: the switch leaves the terminal on the previous look's colours.
#[test]
fn the_new_look_has_a_terminal_palette() {
    let root = std::env::temp_dir().join(format!("eclipse-upstream-foot-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    install(&root);
    let ini = root.join("root/.config/foot/foot.gnome.ini");
    let text = fs::read_to_string(&ini).unwrap_or_else(|e| panic!("{}: {e}", ini.display()));
    assert!(text.contains("[colors-dark]"), "{text}");
    assert!(text.contains("background=1d1d1d"), "{text}");
    let _ = fs::remove_dir_all(&root);
}
