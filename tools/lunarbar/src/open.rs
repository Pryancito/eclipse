//! What opens a file when it is double-clicked, and the command line that does
//! it.
//!
//! On a desktop Linux this is `xdg-open`, which reads `mimeapps.list`, asks the
//! shared-mime-info database what a file is and launches the `.desktop` entry
//! that claims the type. **None of that exists in this image**: there is no
//! `xdg-open` binary, no `/usr/share/mime` database and no `mimeapps.list`, so
//! there is nothing to defer to and nothing to read.
//!
//! So the table below is the whole mechanism, and it is keyed on the file
//! EXTENSION rather than on a sniffed MIME type. That is weaker than
//! shared-mime-info in exactly one way — a file with the wrong extension opens
//! in the wrong program — and stronger in the one that matters here: it needs
//! no database, no cache to regenerate and no daemon, and it gives the same
//! answer in the browser and in a unit test.
//!
//! `/etc/eclipse/openers` overrides and extends it, one `ext=command` line per
//! type, so a program added to the image after this file was written does not
//! need a new build to become the default for its files. The `%f` in a command
//! is the path, already shell-quoted; a command without one gets it appended.

/// What the built-in table opens, by lower-cased extension.
///
/// Two commands carry most of it, and both are image wrappers rather than
/// programs, for the same reason `eclipse-terminal` is one: what is installed
/// varies. `eclipse-edit` is the editor in a terminal (nano if the image has
/// it, busybox `vi` otherwise — busybox always has `vi`, so that path cannot
/// fail), and `eclipse-firefox` is the image's only renderer of HTML, PDF, SVG
/// and images, so a browser tab is the viewer. The wrapper, never `firefox`
/// itself: it is what picks software WebRender over NVK on the GPU-less path
/// (plain `firefox` can draw a black window there) and what finds
/// `firefox-esr` on an image that has only that.
///
/// An entry naming a program that is not installed is not silently launched:
/// `lunarfile` checks the command exists before spawning and says so on the
/// panel, because a file that "opens" into a shell exiting 127 with nothing on
/// screen is the worst of the three outcomes.
pub const BUILTIN: &[(&str, &str)] = &[
    // Text, configuration and source: the editor, in a terminal.
    ("txt", "eclipse-edit %f"),
    ("md", "eclipse-edit %f"),
    ("log", "eclipse-edit %f"),
    ("conf", "eclipse-edit %f"),
    ("cfg", "eclipse-edit %f"),
    ("ini", "eclipse-edit %f"),
    ("toml", "eclipse-edit %f"),
    ("yml", "eclipse-edit %f"),
    ("yaml", "eclipse-edit %f"),
    ("json", "eclipse-edit %f"),
    ("rs", "eclipse-edit %f"),
    ("c", "eclipse-edit %f"),
    ("h", "eclipse-edit %f"),
    ("py", "eclipse-edit %f"),
    ("sh", "eclipse-edit %f"),
    ("service", "eclipse-edit %f"),
    ("desktop", "eclipse-edit %f"),
    // The web browser takes the web's formats.
    ("html", "eclipse-firefox %f"),
    ("htm", "eclipse-firefox %f"),
    ("pdf", "eclipse-firefox %f"),
    ("svg", "eclipse-firefox %f"),
    ("xml", "eclipse-firefox %f"),
    // Images: Firefox again, there being no image viewer in the image.
    ("png", "eclipse-firefox %f"),
    ("jpg", "eclipse-firefox %f"),
    ("jpeg", "eclipse-firefox %f"),
    ("gif", "eclipse-firefox %f"),
    ("bmp", "eclipse-firefox %f"),
    ("webp", "eclipse-firefox %f"),
    // Audio: wavplay is this tree's own player; mpg123 plays the boot chime.
    ("wav", "wavplay %f"),
    ("mp3", "mpg123 %f"),
    ("ogg", "mpg123 %f"),
];

/// The program a command template runs, for the "is it even installed?" check.
///
/// The first word, with any leading path kept: `apps::executable_exists`
/// resolves both an absolute path and a bare name on `PATH`. A template that is
/// nothing but whitespace has no program, and a shell operator as the first
/// word (`VAR=x prog`, a pipeline) is not resolvable as one either — those are
/// left to the shell, which is the one case where a 127 is the honest answer.
pub fn program_of(template: &str) -> Option<&str> {
    let first = template.split_whitespace().next()?;
    if first.contains('=') || first.starts_with('(') {
        return None;
    }
    Some(first)
}

/// The lower-cased extension of `name`, if it has one.
///
/// A leading dot is the whole name of a dotfile and not an extension
/// (`.bashrc` is not a `bashrc` file), and a trailing dot leaves nothing. The
/// LAST dot wins, so `archivo.tar.gz` is a `gz`.
pub fn extension_of(name: &str) -> Option<String> {
    let dot = name.rfind('.')?;
    if dot == 0 || dot + 1 == name.len() {
        return None;
    }
    Some(name[dot + 1..].to_ascii_lowercase())
}

/// Parse an `/etc/eclipse/openers` file: `ext=command` per line, `#` comments
/// and blank lines ignored.
///
/// A later line for the same extension wins, so the file can be appended to.
/// The extension is accepted with or without a leading dot and folded to lower
/// case, because a hand-written file will have both. A line with no `=`, an
/// empty extension or an empty command is skipped rather than stored: a
/// half-parsed entry would make the file "open" into nothing.
pub fn parse_openers(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((ext, cmd)) = line.split_once('=') else {
            continue;
        };
        let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
        let cmd = cmd.trim();
        if ext.is_empty() || cmd.is_empty() {
            continue;
        }
        out.retain(|(e, _)| e != &ext);
        out.push((ext, cmd.to_string()));
    }
    out
}

/// The command template for `name`: the overrides first, then the built-in
/// table, then nothing.
pub fn handler_for(name: &str, overrides: &[(String, String)]) -> Option<String> {
    let ext = extension_of(name)?;
    if let Some((_, cmd)) = overrides.iter().find(|(e, _)| *e == ext) {
        return Some(cmd.clone());
    }
    BUILTIN
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, c)| (*c).to_string())
}

/// The shell command that opens `path` with `template`.
///
/// `%f` is replaced by the **quoted** path (`files::shell_quote`), and a
/// template with no `%f` gets it appended — which is what most commands look
/// like when written by hand. Every `%f` is substituted, not just the first: a
/// template that mentions the file twice would otherwise be handed one path and
/// a literal `%f`.
pub fn open_command(path: &str, template: &str) -> String {
    let quoted = crate::files::shell_quote(path);
    if template.contains("%f") {
        template.replace("%f", &quoted)
    } else {
        format!("{template} {quoted}")
    }
}

/// Load `/etc/eclipse/openers`, or nothing when it is absent or unreadable.
pub fn load_openers() -> Vec<(String, String)> {
    std::fs::read_to_string("/etc/eclipse/openers")
        .map(|t| parse_openers(&t))
        .unwrap_or_default()
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_is_the_last_one_and_a_dotfile_has_none() {
        assert_eq!(extension_of("notas.txt").as_deref(), Some("txt"));
        assert_eq!(extension_of("Informe.PDF").as_deref(), Some("pdf"));
        // The last dot wins: a `.tar.gz` is a gz, which is what opens it.
        assert_eq!(extension_of("archivo.tar.gz").as_deref(), Some("gz"));
        // A dotfile's name is not an extension. `.bashrc` is not a `bashrc`
        // file, and treating it as one would open every dotfile in whatever
        // claimed its name.
        assert_eq!(extension_of(".bashrc"), None);
        assert_eq!(extension_of(".gitignore"), None);
        // A trailing dot leaves nothing after it.
        assert_eq!(extension_of("informe."), None);
        // And no dot at all is no extension.
        assert_eq!(extension_of("Makefile"), None);
        assert_eq!(extension_of(""), None);
        assert_eq!(extension_of("."), None);
        assert_eq!(extension_of(".."), None);
        // A dotfile WITH an extension has that extension.
        assert_eq!(extension_of(".config.json").as_deref(), Some("json"));
    }

    #[test]
    fn the_path_reaches_the_command_quoted_and_every_mention_of_it() {
        // The reason this function exists rather than a `format!` at the call
        // site: the path goes through `/bin/sh -c`.
        assert_eq!(
            open_command("/home/m/a.txt", "eclipse-edit %f"),
            "eclipse-edit '/home/m/a.txt'"
        );
        assert_eq!(
            open_command("/home/m/x'; reboot; '.txt", "eclipse-firefox %f"),
            r#"eclipse-firefox '/home/m/x'\''; reboot; '\''.txt'"#
        );
        // A template with no %f gets the path appended, which is what a
        // hand-written `/etc/eclipse/openers` line looks like.
        assert_eq!(open_command("/a b", "mpg123"), "mpg123 '/a b'");
        // Every %f, not just the first: one left as a literal would reach the
        // program as the string "%f" and be opened as a file of that name.
        assert_eq!(open_command("/a", "cmp %f %f"), "cmp '/a' '/a'");
        // And the quoting is the same function the browser uses everywhere.
        assert_eq!(
            open_command("/a'b", "p %f"),
            format!("p {}", crate::files::shell_quote("/a'b"))
        );
    }

    #[test]
    fn an_openers_file_overrides_the_table_and_a_later_line_wins() {
        let text = "\
# el visor de imágenes nuevo
png = eclipse-view %f
.pdf=zathura %f
PNG = eclipse-view2 %f

   txt   =   nano-wrapper %f
mal
=sin extension
vacio=
";
        let ov = parse_openers(text);
        // Comments, blank lines and malformed lines are skipped, not stored.
        assert!(!ov.iter().any(|(e, _)| e.is_empty()));
        assert!(!ov.iter().any(|(_, c)| c.is_empty()));
        assert!(!ov.iter().any(|(e, _)| e == "mal"));
        // A leading dot and any casing are accepted; both end up as `pdf`.
        assert_eq!(handler_for("x.pdf", &ov).as_deref(), Some("zathura %f"));
        // The later line for the same extension wins, so the file can be
        // appended to, and `PNG` is the same extension as `png`.
        assert_eq!(
            handler_for("x.png", &ov).as_deref(),
            Some("eclipse-view2 %f")
        );
        assert_eq!(ov.iter().filter(|(e, _)| e == "png").count(), 1);
        // Whitespace around either side is trimmed.
        assert_eq!(
            handler_for("x.txt", &ov).as_deref(),
            Some("nano-wrapper %f")
        );
        // And the built-in table still answers for everything the file did not
        // mention.
        assert_eq!(handler_for("x.wav", &ov).as_deref(), Some("wavplay %f"));
        // An empty file changes nothing.
        assert!(parse_openers("").is_empty());
        assert!(parse_openers("# solo un comentario\n\n").is_empty());
    }

    #[test]
    fn a_file_nothing_claims_has_no_handler() {
        // The browser says "no program for .xyz" rather than spawning a shell
        // that exits 127 behind a screen with nothing on it.
        assert_eq!(handler_for("algo.xyz", &[]), None);
        assert_eq!(handler_for("Makefile", &[]), None);
        assert_eq!(handler_for(".bashrc", &[]), None);
        // Case does not matter on the way in either.
        assert_eq!(
            handler_for("FOTO.JPG", &[]).as_deref(),
            Some("eclipse-firefox %f")
        );
    }

    #[test]
    fn every_builtin_entry_is_a_lower_case_extension_naming_one_program() {
        // A capital in a key is an entry that can never match, because
        // `extension_of` folds its answer; a duplicate key is a second entry
        // nothing can reach, since `find` stops at the first.
        let mut seen: Vec<&str> = Vec::new();
        for (ext, cmd) in BUILTIN {
            assert_eq!(*ext, ext.to_ascii_lowercase(), "{ext} can never match");
            assert!(!ext.starts_with('.'), "{ext} has a leading dot");
            assert!(!ext.is_empty());
            assert!(!seen.contains(ext), "{ext} is in the table twice");
            seen.push(ext);
            // Every command must take the file, or `open_command` appends it
            // and a viewer gets launched with no argument at all.
            assert!(cmd.contains("%f"), "{ext} -> {cmd} never gets the file");
            assert!(program_of(cmd).is_some(), "{ext} -> {cmd} names no program");
        }
        // The four programs the table is allowed to name, so an entry pointing
        // at something this image does not ship is a visible line in the diff.
        // `eclipse-firefox` and not `firefox`: every activation path in the
        // image is required to go through that wrapper (see
        // `xtask/src/linux/desktop.rs`), and bypassing it loses both the
        // renderer choice and the ESR fallback.
        let progs: Vec<&str> = BUILTIN.iter().filter_map(|(_, c)| program_of(c)).collect();
        for p in &progs {
            assert!(
                ["eclipse-edit", "eclipse-firefox", "wavplay", "mpg123"].contains(p),
                "{p} is not one of the image's openers"
            );
        }
        // And the browser is never named directly, whatever the extension.
        assert!(
            !progs.contains(&"firefox") && !progs.contains(&"firefox-esr"),
            "una entrada se salta el envoltorio eclipse-firefox: {progs:?}"
        );
    }

    #[test]
    fn the_program_of_a_template_is_its_first_word() {
        assert_eq!(program_of("eclipse-firefox %f"), Some("eclipse-firefox"));
        assert_eq!(program_of("  firefox   %f  "), Some("firefox"));
        assert_eq!(
            program_of("/usr/local/bin/eclipse-edit %f"),
            Some("/usr/local/bin/eclipse-edit")
        );
        assert_eq!(program_of("eclipse-firefox"), Some("eclipse-firefox"));
        // Nothing to run.
        assert_eq!(program_of(""), None);
        assert_eq!(program_of("   "), None);
        // A shell construct is not a program name, so it is not checked for
        // existence -- the shell is the right thing to hand those to.
        assert_eq!(program_of("MOZ_ENABLE_WAYLAND=1 firefox %f"), None);
        assert_eq!(program_of("(cd /tmp && p) %f"), None);
    }
}
