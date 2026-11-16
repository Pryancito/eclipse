//! The `abyss` look: the lunar apps dressed as the Abyss session around them.
//!
//! The lunar clients draw it themselves (`tools/lunarbar/src/glass.rs`: warm
//! black ground made translucent so Abyss blurs behind it, rounded cells,
//! white ink and gold on the selected label only). Here are the pieces they
//! cannot draw, so that switching to it under labwc is a whole look and so
//! `eclipse-look` can come back out of it: the labwc window frame and foot's
//! palette. No GTK stylesheet: GTK stays on stock Adwaita-dark, which is
//! neutral enough beside it.
//!
//! The values are those of Abyss's style guide (`desktop/docs/STYLE.md`):
//!
//! | role | hex |
//! |---|---|
//! | ground | `#0b0906` |
//! | card / titlebar | `#14120f` |
//! | hairline | `#282724` (white at .12) |
//! | rim of what is focused | `#3c3a38` (white at .20) |
//! | text | `#ffffff`, secondary `#a7a6a5` |
//! | accent | `#f2c33c`, as text `#f5cf5c` |

use std::{fs, path::Path};

/// The window theme and the terminal palette this look needs.
pub(super) fn install(rootfs: &Path) {
    write_theme(rootfs);
    write_foot(rootfs);
}

/// `Abyss-Dark`: the same keys as Eclipse's other themes. Focus is a
/// clearer titlebar and a crisp white rim, never a gold frame: gold goes on
/// the focused title's text only.
fn write_theme(rootfs: &Path) {
    let dir = rootfs.join("usr/share/themes/Abyss-Dark/openbox-3");
    let _ = fs::create_dir_all(&dir);
    fs::write(
        dir.join("themerc"),
        b"# Eclipse OS - abyss: the Abyss session's warm-black glass for labwc/openbox.\n\
          border.width: 1\n\
          padding.width: 8\n\
          padding.height: 6\n\
          \n\
          window.active.border.color: #3c3a38\n\
          window.inactive.border.color: #282724\n\
          window.active.title.bg.color: #1d1b18\n\
          window.inactive.title.bg.color: #0b0906\n\
          window.active.label.text.color: #f5cf5c\n\
          window.inactive.label.text.color: #a7a6a5\n\
          window.active.button.unpressed.image.color: #a7a6a5\n\
          window.active.button.pressed.image.color: #f2c33c\n\
          window.active.button.hover.image.color: #ffffff\n\
          window.inactive.button.unpressed.image.color: #5c5a57\n\
          window.active.button.close.unpressed.image.color: #f58a72\n\
          window.active.button.close.hover.image.color: #f8a896\n\
          window.inactive.button.close.unpressed.image.color: #5c5a57\n\
          window.label.text.justify: Center\n\
          \n\
          menu.title.bg.color: #14120f\n\
          menu.title.text.color: #ffffff\n\
          menu.items.bg.color: #0b0906\n\
          menu.items.text.color: #ffffff\n\
          menu.items.disabled.text.color: #5c5a57\n\
          menu.items.active.bg.color: #282623\n\
          menu.items.active.text.color: #f5cf5c\n\
          menu.separator.color: #282724\n\
          menu.separator.padding.height: 4\n\
          \n\
          osd.bg.color: #0b0906\n\
          osd.border.color: #3c3a38\n\
          osd.border.width: 1\n\
          osd.label.text.color: #ffffff\n\
          osd.window-switcher.style-classic.width: 640\n\
          osd.window-switcher.style-classic.padding: 8\n\
          osd.window-switcher.style-classic.item.padding.x: 12\n\
          osd.window-switcher.style-classic.item.padding.y: 4\n\
          osd.window-switcher.style-classic.item.active.border.width: 1\n\
          osd.window-switcher.style-classic.item.icon.size: 20\n\
          osd.window-switcher.preview.border.width: 2\n\
          osd.window-switcher.preview.border.color: #3c3a38\n\
          osd.workspace-switcher.boxes.width: 16\n\
          osd.workspace-switcher.boxes.height: 16\n\
          osd.workspace-switcher.boxes.border.width: 1\n\
          \n\
          # The guide's window shadow: deep and soft, as Abyss draws it.\n\
          window.active.shadow.size: 40\n\
          window.active.shadow.color: #000000 85\n\
          window.inactive.shadow.size: 24\n\
          window.inactive.shadow.color: #000000 60\n",
    )
    .unwrap();
    // The tecno buttons' glyphs: plain XBM, coloured by the keys above.
    for (name, bits) in super::tecno::BUTTONS {
        fs::write(dir.join(name), bits).unwrap();
    }
}

/// `foot.abyss.ini`, which `eclipse-look` copies over `foot.ini`.
fn write_foot(rootfs: &Path) {
    let dir = rootfs.join("root/.config/foot");
    let _ = fs::create_dir_all(&dir);
    fs::write(dir.join("foot.abyss.ini"), super::foot_ini(FOOT_ABYSS)).unwrap();
}

/// The terminal palette: warm neutrals, the gold for yellow, the guide's
/// coral for red, so "bad" and "selected" mean the same colours as in the
/// apps.
const FOOT_ABYSS: &str = "\
background=0b0906\nforeground=f4f1ea\n\
regular0=1c1913\nregular1=f58a72\nregular2=8fd69c\nregular3=f2c33c\n\
regular4=8fb4d9\nregular5=c9a0dc\nregular6=8fd1c4\nregular7=d8d4cc\n\
bright0=5c5a57\nbright1=f8a896\nbright2=b2e6bc\nbright3=f5cf5c\n\
bright4=b1cbe6\nbright5=dcbfe8\nbright6=b0e2d8\nbright7=ffffff\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_and_the_palette_land_where_the_switch_looks_for_them() {
        let dir = std::env::temp_dir().join(format!("abyss-look-xtask-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        install(&dir);
        let rc = fs::read_to_string(dir.join("usr/share/themes/Abyss-Dark/openbox-3/themerc"))
            .expect("themerc");
        assert!(rc.contains("window.active.title.bg.color: #1d1b18"));
        assert!(dir.join("usr/share/themes/Abyss-Dark/openbox-3/close.xbm").is_file());
        let ini = fs::read_to_string(dir.join("root/.config/foot/foot.abyss.ini")).expect("ini");
        assert!(ini.contains("background=0b0906"));
        let _ = fs::remove_dir_all(&dir);
    }
}
