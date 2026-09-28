//! Which look the desktop is wearing: KDE's Breeze Dark, or Eclipse's own
//! violet. Persisted by `eclipse-look` in `/etc/eclipse/look` (and applied at
//! boot by eclipse-init), so every client agrees without an IPC bus.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// Windows 11 dark: one bottom taskbar with its buttons CENTRED,
    /// `#202020` ground, `#0078d4` accent.
    Win11,
    /// KDE Breeze Dark: one bottom panel, grey ground, `#3daee9` accent.
    Kde,
    /// Eclipse's own: two bars, blue-black ground, blue accent. The default,
    /// and what the image ships.
    Eclipse,
}

impl Look {
    /// The name as `eclipse-look` writes it, or as `$ECLIPSE_LOOK` carries it.
    ///
    /// Case-folded ONCE. The arms used to spell out the capitalisations by hand
    /// and they did not agree with each other: `KDE` and `Breeze` were accepted
    /// but `Kde` was not, `Win11` but not `WIN11`, `Eclipse` but not `ECLIPSE`.
    /// A `/etc/eclipse/look` written by hand in any other casing fell through to
    /// the default, which looks exactly like the setting not having been saved.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "win11" | "windows" | "windows11" => Some(Look::Win11),
            "kde" | "breeze" => Some(Look::Kde),
            "eclipse" => Some(Look::Eclipse),
            _ => None,
        }
    }

    /// `$ECLIPSE_LOOK` (a launcher override) wins, then `/etc/eclipse/look`,
    /// else the default.
    ///
    /// **Call this ONCE per process and keep the answer.** It reads a file, and
    /// `eclipse-look` rewrites that file when the user switches looks, so two
    /// calls can return two different looks: a client that takes its palette
    /// from one call and its geometry from another draws a panel in one look's
    /// colours with the other look's layout.
    pub fn current() -> Self {
        resolve(std::env::var("ECLIPSE_LOOK").ok().as_deref(), file_look)
    }
}

impl Default for Look {
    /// Eclipse's own look. Not a preference: the image's wallpaper, icons and
    /// accent are all built for this one, so a client defaulting to another
    /// comes up looking like it belongs to a different desktop.
    fn default() -> Self {
        Look::Eclipse
    }
}

/// `$ECLIPSE_LOOK` beats the settings file beats the default.
///
/// The environment wins because it is how a keybind or a test starts one client
/// in another look without touching the file every other client reads. An
/// unrecognised value in it falls through to the file rather than to the
/// default, so a typo in a launcher does not override a saved setting.
///
/// `file` is a closure so the precedence can be checked without a disk: the file
/// is only read when the environment has nothing to say.
fn resolve(env: Option<&str>, file: impl FnOnce() -> Option<Look>) -> Look {
    if let Some(l) = env.and_then(Look::from_name) {
        return l;
    }
    file().unwrap_or_default()
}

/// First `look=` line of `/etc/eclipse/look`, comments skipped.
fn file_look() -> Option<Look> {
    let text = std::fs::read_to_string("/etc/eclipse/look").ok()?;
    look_from_text(&text)
}

/// The look named by the first `look=` line of a settings file.
///
/// The FIRST one wins even when its value is unknown, so a typo is a visible
/// fallback to the default rather than a silent jump to whatever a later line
/// happens to say -- the file is one setting, and two `look=` lines in it are
/// already a mistake someone needs to see.
pub fn look_from_text(text: &str) -> Option<Look> {
    // Blank lines and `#` comments fall out on their own: neither can start with
    // `look=`. Skipping them explicitly was three lines that changed nothing.
    let first = text.lines().find_map(|l| l.trim().strip_prefix("look="))?;
    Look::from_name(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_look_name_is_read_whatever_its_capitalisation() {
        // The arms used to name the capitalisations one by one and disagreed:
        // `KDE` worked and `Kde` did not, so a hand-written /etc/eclipse/look
        // fell through to the default and looked like a setting that had not
        // saved.
        for (name, want) in [
            ("kde", Look::Kde),
            ("KDE", Look::Kde),
            ("Kde", Look::Kde),
            ("kDe", Look::Kde),
            ("breeze", Look::Kde),
            ("Breeze", Look::Kde),
            ("BREEZE", Look::Kde),
            ("win11", Look::Win11),
            ("Win11", Look::Win11),
            ("WIN11", Look::Win11),
            ("windows", Look::Win11),
            ("Windows11", Look::Win11),
            ("eclipse", Look::Eclipse),
            ("Eclipse", Look::Eclipse),
            ("ECLIPSE", Look::Eclipse),
        ] {
            assert_eq!(Look::from_name(name), Some(want), "{name}");
        }
        // Whitespace around it too: a file written with a trailing newline or a
        // space after the `=` is the normal case, not the exception.
        assert_eq!(Look::from_name("  kde\n"), Some(Look::Kde));
        assert_eq!(Look::from_name("\tEclipse "), Some(Look::Eclipse));
        // And nothing else is a look.
        for name in [
            "", "  ", "plasma", "gnome", "kde11", "win", "look=kde", "#kde",
        ] {
            assert_eq!(Look::from_name(name), None, "{name:?}");
        }
    }

    #[test]
    fn the_settings_file_is_read_the_way_eclipse_look_writes_it() {
        assert_eq!(look_from_text("look=kde\n"), Some(Look::Kde));
        // Indented by hand, or with the value padded: both are what a person
        // editing /etc/eclipse/look in vi actually leaves behind.
        assert_eq!(look_from_text("  look=kde\n"), Some(Look::Kde));
        assert_eq!(look_from_text("\tlook=win11"), Some(Look::Win11));
        assert_eq!(look_from_text("look=kde  \n"), Some(Look::Kde));
        // Comments and blank lines skipped, as the format allows.
        assert_eq!(
            look_from_text("# escrito por eclipse-look\n\nlook=win11\n"),
            Some(Look::Win11)
        );
        // The first `look=` wins, even when what it says is not a look: a typo
        // has to be visible as the default, not silently overridden by a line
        // further down that nobody is looking at.
        assert_eq!(look_from_text("look=plsma\nlook=kde\n"), None);
        assert_eq!(look_from_text("look=kde\nlook=win11\n"), Some(Look::Kde));
        // A file with no setting in it, or with other settings, is no look.
        assert_eq!(look_from_text(""), None);
        assert_eq!(look_from_text("# nada\n"), None);
        assert_eq!(look_from_text("theme=kde\n"), None);
        assert_eq!(look_from_text("LOOK=kde\n"), None);
        // A value with trailing rubbish is not a look either: `look=kde # x`
        // must not be read as kde, because then the comment becomes part of
        // whatever the next reader parses out of it.
        assert_eq!(look_from_text("look=kde # comentario\n"), None);
    }

    #[test]
    fn the_default_look_is_the_one_the_image_ships() {
        // Not a preference: the image's wallpaper, panel and icons are all built
        // for this one, and a client that defaulted to another would come up
        // with a different accent from everything around it.
        assert_eq!(Look::default(), Look::Eclipse);
        // With nothing set and no file, that is what a client gets.
        assert_eq!(resolve(None, || None), Look::Eclipse);
        assert_eq!(resolve(Some(""), || None), Look::Eclipse);
    }

    #[test]
    fn the_environment_beats_the_file_beats_the_default() {
        // `$ECLIPSE_LOOK` is how a keybind or a test starts one client in
        // another look without rewriting the file every other client reads.
        assert_eq!(resolve(Some("win11"), || Some(Look::Kde)), Look::Win11);
        assert_eq!(resolve(None, || Some(Look::Kde)), Look::Kde);
        assert_eq!(resolve(Some("KDE"), || Some(Look::Win11)), Look::Kde);
        // An unrecognised value in the environment falls through to the FILE and
        // not to the default, so a typo in a launcher does not silently discard
        // the setting the user saved.
        assert_eq!(resolve(Some("plsma"), || Some(Look::Kde)), Look::Kde);
        assert_eq!(resolve(Some("plsma"), || None), Look::default());
        // And the file is not read at all when the environment answers: it is a
        // disk read on the startup path of every client.
        let mut read = false;
        let l = resolve(Some("win11"), || {
            read = true;
            None
        });
        assert_eq!(l, Look::Win11);
        assert!(
            !read,
            "the file was read even though the environment answered"
        );
    }
}
