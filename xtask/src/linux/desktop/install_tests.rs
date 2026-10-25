//! What [`super::install`] leaves in the rootfs, read back as a whole.
//!
//! The tests next door reach for one writer at a time, and `install` itself
//! was never among them: nine of the writers it calls -- the Xorg config and
//! the two X session scripts, the terminal, view, xkbmap and memwatch
//! wrappers, the Xfce defaults, the fallback icons, the wallpaper -- and
//! `sha512_of` under them were reached by no test at all. The sweeps here
//! install the whole desktop into a directory and then ask questions of
//! everything that landed in it, so the next writer is covered the day it is
//! added rather than the day somebody remembers to add a test for it.
//!
//! The other half is names. A launcher's path is written in one file and read
//! in another -- a menu entry, a keybind, a `.desktop` row, a service in
//! `linux/mod.rs`, the panel's own opener table in `tools/lunarbar` -- and
//! nothing makes the two agree: a renamed wrapper still compiles, still
//! installs, and simply does nothing when it is clicked. Same for the three
//! looks `eclipse-look` switches between, which have to line up across the
//! guard that accepts them, the theme each one maps to, the `sed` that puts
//! that theme in `rc.xml`, the palette copied over `foot.ini`, and the usage
//! line that tells a person which names exist.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// One whole desktop install, shared by every sweep below.
///
/// The wallpaper is seeded before the install so the 4 MiB render is skipped
/// (see [`super::write_wallpaper`], whose own test is the one that renders):
/// these sweeps are about names, modes and contents, and the picture costs
/// more than all of them together.
struct Installed {
    root: PathBuf,
    /// Every file under the root, as a path relative to it, sorted.
    files: Vec<String>,
}

fn installed() -> &'static Installed {
    static ONCE: std::sync::OnceLock<Installed> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let root =
            std::env::temp_dir().join(format!("eclipse-desktop-sweep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let bg = root.join("usr/share/backgrounds/eclipse");
        fs::create_dir_all(&bg).unwrap();
        fs::write(bg.join("eclipse-night.png"), b"seeded").unwrap();
        install(&root);
        let mut files = Vec::new();
        walk(&root, &root, &mut files);
        files.sort();
        Installed { root, files }
    })
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(root, &p, out);
        } else {
            out.push(p.strip_prefix(root).unwrap().to_string_lossy().to_string());
        }
    }
}

impl Installed {
    fn text(&self, rel: &str) -> String {
        let p = self.root.join(rel);
        fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    fn mode(&self, rel: &str) -> u32 {
        fs::metadata(self.root.join(rel))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    /// The files that are meant to be run: the wrappers in `/usr/local/bin`
    /// and the two scripts `startx` sources.
    fn scripts(&self) -> Vec<&str> {
        self.files
            .iter()
            .map(String::as_str)
            .filter(|f| {
                f.starts_with("usr/local/bin/") || *f == "root/.xinitrc" || *f == "root/.xserverrc"
            })
            .collect()
    }

    /// The basenames of the wrappers the desktop itself installs.
    fn wrappers(&self) -> BTreeSet<&str> {
        self.files
            .iter()
            .filter_map(|f| f.strip_prefix("usr/local/bin/"))
            .collect()
    }

    /// Every `/usr/local/bin/<name>` named inside a file the install wrote,
    /// and which files named it.
    fn paths_named(&self) -> BTreeMap<String, Vec<&str>> {
        let mut out: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for rel in &self.files {
            let Ok(text) = fs::read_to_string(self.root.join(rel)) else {
                continue; // the wallpaper and the icons are not text
            };
            let mut at = 0;
            while let Some(found) = text[at..].find("/usr/local/bin/") {
                let start = at + found + "/usr/local/bin/".len();
                let end = text[start..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                    .map(|n| start + n)
                    .unwrap_or(text.len());
                if end > start {
                    let e = out.entry(text[start..end].to_string()).or_default();
                    if !e.contains(&rel.as_str()) {
                        e.push(rel);
                    }
                }
                at = start;
            }
        }
        out
    }
}

/// A source file of the image build, read at run time rather than through
/// `include_str!`: these are cross-checks, and a cross-check that forces a
/// rebuild of the thing it checks is one more way to be stale.
fn source(rel: &str) -> String {
    let p = crate::PROJECT_DIR.join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

// ---------------------------------------------------------------------------
// The looks
// ---------------------------------------------------------------------------

/// The looks `eclipse-look`'s own guard accepts, read out of the installed
/// script. The test next door iterates a list typed into the test instead, so
/// a fourth look wired up by halves passes it; the whole point here is that
/// nothing is written down twice.
fn looks_accepted(script: &str) -> Vec<String> {
    let line = script
        .lines()
        .find(|l| l.contains("case \"$1\" in") && l.contains("return 0"))
        .expect("look_ok's case line is gone from eclipse-look");
    let list = line
        .split(" in ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("look_ok's case line changed shape");
    list.split('|').map(|s| s.trim().to_string()).collect()
}

/// What `theme_for` maps each look to, `*` being its fallback.
fn themes_by_look(script: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut inside = false;
    for line in script.lines() {
        if line.starts_with("theme_for() {") {
            inside = true;
            continue;
        }
        if inside && line == "}" {
            break;
        }
        if !inside {
            continue;
        }
        let t = line.trim();
        if let Some((look, rest)) = t.split_once(") echo ") {
            if let Some(theme) = rest.strip_suffix(" ;;") {
                out.insert(look.to_string(), theme.to_string());
            }
        }
    }
    assert!(!out.is_empty(), "theme_for maps nothing:\n{script}");
    out
}

/// The themes `apply`'s `sed` can rewrite `rc.xml` AWAY from.
fn themes_the_sed_replaces(script: &str) -> BTreeSet<String> {
    script
        .lines()
        .filter_map(|l| l.split_once("s|<name>"))
        .filter_map(|(_, rest)| rest.split_once("</name>"))
        .map(|(name, _)| name.to_string())
        .collect()
}

fn theme_of(themes: &BTreeMap<String, String>, look: &str) -> String {
    themes
        .get(look)
        .or_else(|| themes.get("*"))
        .unwrap_or_else(|| panic!("theme_for says nothing about {look} and has no fallback"))
        .clone()
}

/// Switching to a look that has no theme leaves labwc on its built-in
/// defaults, and one that has no palette leaves the terminal on the previous
/// look's colours. Both are silent: `eclipse-look` prints the name and exits 0.
#[test]
fn every_look_the_switcher_offers_has_a_theme_and_a_palette() {
    let d = installed();
    let script = d.text("usr/local/bin/eclipse-look");
    let themes = themes_by_look(&script);
    let accepted = looks_accepted(&script);
    assert!(
        accepted.len() >= 2,
        "a switcher with one look is not a switcher: {accepted:?}"
    );
    for look in &accepted {
        let theme = theme_of(&themes, look);
        let rc = format!("usr/share/themes/{theme}/openbox-3/themerc");
        assert!(
            d.files.iter().any(|f| *f == rc),
            "the {look} look asks for the {theme} theme and nothing installs {rc}"
        );
        let ini = format!("root/.config/foot/foot.{look}.ini");
        assert!(
            d.files.iter().any(|f| *f == ini),
            "the {look} look has no terminal palette at {ini}"
        );
    }
}

/// `apply` rewrites `rc.xml` with one `sed -e` per theme name it might find
/// there. A theme the `sed` cannot match is a one-way trip: you can switch
/// into that look and never out of it.
#[test]
fn the_switch_can_come_back_from_every_look_it_can_set() {
    let d = installed();
    let script = d.text("usr/local/bin/eclipse-look");
    let themes = themes_by_look(&script);
    let replaced = themes_the_sed_replaces(&script);
    for look in looks_accepted(&script) {
        let theme = theme_of(&themes, &look);
        assert!(
            replaced.contains(&theme),
            "eclipse-look can set {theme} for the {look} look but its sed only \
             rewrites {replaced:?}, so the switch out of {look} does nothing"
        );
    }
}

/// The usage line is the only place a person is told which names exist.
#[test]
fn the_usage_line_names_the_looks_the_script_accepts() {
    let d = installed();
    let script = d.text("usr/local/bin/eclipse-look");
    let usage = script
        .lines()
        .find(|l| l.contains("usage: eclipse-look"))
        .expect("eclipse-look no longer prints a usage line");
    let listed: BTreeSet<String> = usage
        .split_once('[')
        .and_then(|(_, rest)| rest.split_once(']'))
        .expect("the usage line no longer lists its arguments")
        .0
        .split('|')
        .filter(|a| *a != "--boot")
        .map(str::to_string)
        .collect();
    let accepted: BTreeSet<String> = looks_accepted(&script).into_iter().collect();
    assert_eq!(
        listed, accepted,
        "the usage line and look_ok disagree about which looks there are"
    );
}

/// `/etc/eclipse/look` is what `--boot` reads on every boot. A value the guard
/// refuses falls back to `eclipse`, so a typo here is a look silently ignored.
#[test]
fn the_look_the_image_ships_is_one_the_switcher_accepts() {
    let d = installed();
    let conf = d.text("etc/eclipse/look");
    let shipped = conf
        .lines()
        .find_map(|l| l.trim().strip_prefix("look="))
        .expect("etc/eclipse/look no longer says look=")
        .trim()
        .to_string();
    let script = d.text("usr/local/bin/eclipse-look");
    assert!(
        looks_accepted(&script).contains(&shipped),
        "the image ships look={shipped}, which eclipse-look does not accept"
    );
    let theme = theme_of(&themes_by_look(&script), &shipped);
    let rc = d.text("root/.config/labwc/rc.xml");
    assert!(
        rc.contains(&format!("<name>{theme}</name>")),
        "the session starts on the {shipped} look, whose theme is {theme}, but \
         rc.xml names another one"
    );
}

/// Every theme a look can choose has to name every colour the others name.
/// A theme silent about one of them does not inherit the look it came from --
/// openbox reads each themerc whole -- it falls through to openbox's own
/// palette, so one widget comes out in a colour belonging to no look at all.
///
/// Only the colours. The three deliberately disagree about
/// `window.label.text.justify`, because each imitates a desktop that aligns
/// titles its own way and Eclipse's own takes openbox's default on purpose
/// (`each_theme_keeps_the_title_alignment_of_the_desktop_it_imitates`, next
/// door, is what pins that). A default alignment is a defensible thing to
/// inherit; a default colour in the middle of a palette is not.
#[test]
fn the_themes_a_look_can_choose_all_name_the_same_colours() {
    let d = installed();
    let script = d.text("usr/local/bin/eclipse-look");
    let themes = themes_by_look(&script);
    let colours = |theme: &str| -> BTreeSet<String> {
        d.text(&format!("usr/share/themes/{theme}/openbox-3/themerc"))
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .filter_map(|l| l.split_once(':'))
            .map(|(k, _)| k.trim().to_string())
            .filter(|k| k.ends_with(".color"))
            .collect()
    };
    let mut names: Vec<&String> = themes.values().collect();
    names.sort();
    names.dedup();
    let first = names[0];
    let want = colours(first);
    assert!(
        want.len() > 15,
        "{first} names almost no colours, so this proves nothing: {want:?}"
    );
    for theme in &names[1..] {
        let got = colours(theme);
        let missing: Vec<&String> = want.difference(&got).collect();
        let extra: Vec<&String> = got.difference(&want).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{theme} and {first} do not name the same colours: {theme} is \
             missing {missing:?} and names {extra:?} that {first} does not, so \
             one of the two looks shows a colour out of openbox's own palette"
        );
    }
}

/// Every palette has to fill every colour slot, for the same reason: foot keeps
/// its built-in colour for a slot the file does not set, so a palette one line
/// short shows one of foot's own colours in the middle of the look.
#[test]
fn the_terminal_palettes_fill_the_same_slots_with_colours_of_their_own() {
    let d = installed();
    let script = d.text("usr/local/bin/eclipse-look");
    let read = |look: &str| -> BTreeMap<String, String> {
        d.text(&format!("root/.config/foot/foot.{look}.ini"))
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .filter(|(k, _)| {
                k == "background"
                    || k == "foreground"
                    || k.starts_with("regular")
                    || k.starts_with("bright")
            })
            .collect()
    };
    let looks = looks_accepted(&script);
    let first = read(&looks[0]);
    assert_eq!(
        first.len(),
        18,
        "a palette is 16 ANSI colours plus a background and a foreground: {first:?}"
    );
    for look in &looks[1..] {
        let got = read(look);
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            first.keys().collect::<Vec<_>>(),
            "the {look} palette does not fill the same slots as the {} one",
            looks[0]
        );
        assert_ne!(
            got, first,
            "the {look} palette is the same colours as the {} one, so the \
             terminal does not follow the look",
            looks[0]
        );
    }
}

/// The two places a person reads the list of looks without running anything:
/// the header of the terminal config they open to change their colours, and
/// the banner at the top of the switcher itself. Both named two of the three
/// -- a look that exists, works, and is mentioned nowhere a reader looks.
#[test]
fn the_notes_in_the_shipped_files_name_every_look_there_is() {
    let d = installed();
    let script = d.text("usr/local/bin/eclipse-look");
    let banner: String = script
        .lines()
        .skip(1)
        .take_while(|l| l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let header: String = d
        .text("root/.config/foot/foot.ini")
        .lines()
        .take_while(|l| l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        header.contains("eclipse-look"),
        "foot.ini no longer says what switches its colours:\n{header}"
    );
    for look in looks_accepted(&script) {
        assert!(
            header.contains(&look),
            "foot.ini's header does not mention the {look} look:\n{header}"
        );
        assert!(
            banner.contains(&look),
            "eclipse-look's own banner does not mention the {look} look:\n{banner}"
        );
    }
}

// ---------------------------------------------------------------------------
// The scripts
// ---------------------------------------------------------------------------

/// A wrapper that is not executable is a menu entry that does nothing, and one
/// without a shebang is run by whatever shell happens to pick it up. Both are
/// one forgotten line in a writer away, and neither fails the build.
#[test]
fn every_script_the_desktop_installs_is_an_executable_shell_script() {
    let d = installed();
    let scripts = d.scripts();
    assert!(
        scripts.len() >= 15,
        "the install stopped writing its wrappers: {scripts:?}"
    );
    for rel in scripts {
        assert_eq!(d.mode(rel), 0o755, "{rel} is not executable");
        let text = d.text(rel);
        assert!(
            text.starts_with("#!/bin/sh\n"),
            "{rel} does not start with a /bin/sh shebang: {:?}",
            text.lines().next().unwrap_or("")
        );
        assert!(text.ends_with('\n'), "{rel} does not end with a newline");
    }
}

/// `sh -n` on everything, rather than on the two or three scripts a test
/// happened to be written for. A quoting mistake inside one of these string
/// literals is invisible in the diff and fatal at boot.
#[test]
fn every_script_the_desktop_installs_parses_as_posix_shell() {
    let d = installed();
    for rel in d.scripts() {
        let st = std::process::Command::new("sh")
            .arg("-n")
            .arg(d.root.join(rel))
            .status()
            .unwrap_or_else(|e| panic!("sh: {e}"));
        assert!(st.success(), "sh -n rejected {rel}");
    }
}

/// The converse of the mode check above: a config file installed executable
/// means a writer copied the wrong tail, and a `.desktop` entry with the bit
/// set is what some launchers refuse to show at all.
#[test]
fn nothing_the_desktop_installs_is_executable_unless_it_is_a_script() {
    let d = installed();
    let scripts: BTreeSet<&str> = d.scripts().into_iter().collect();
    for rel in &d.files {
        if scripts.contains(rel.as_str()) {
            continue;
        }
        assert_eq!(
            d.mode(rel),
            0o644,
            "{rel} is not a script and should not carry its mode"
        );
    }
}

// ---------------------------------------------------------------------------
// The names
// ---------------------------------------------------------------------------

/// A keybind, a menu entry or a `.desktop` row naming a wrapper that nothing
/// installs is a dead click. The desktop writes most of them itself; the rest
/// come from `linux/mod.rs` (the panel's background and the service wrappers)
/// and from `linux/portal.rs` (the portal), which is where this looks next.
#[test]
fn every_program_the_desktop_names_by_its_path_is_one_the_image_installs() {
    let d = installed();
    let mine = d.wrappers();
    let elsewhere = source("xtask/src/linux/mod.rs") + &source("xtask/src/linux/portal.rs");
    for (name, from) in d.paths_named() {
        if mine.contains(name.as_str()) {
            continue;
        }
        assert!(
            elsewhere.contains(&format!("\"{name}\"")),
            "{from:?} names /usr/local/bin/{name}, which neither the desktop \
             nor mod.rs nor portal.rs installs"
        );
    }
}

/// And the other way round: a wrapper nothing names is dead weight in the
/// image, and usually a rename that only landed on one side. Three ways to be
/// named count -- by path, by bare name on `PATH` (which is how `.xinitrc`
/// starts the memory watch and the X cache warm-up), or from `mod.rs`, which
/// writes the init services and the openers.
#[test]
fn every_wrapper_the_desktop_installs_is_reached_from_somewhere() {
    let d = installed();
    let by_path = d.paths_named();
    let bodies: Vec<(&str, String)> = d
        .files
        .iter()
        .filter_map(|rel| {
            fs::read_to_string(d.root.join(rel))
                .ok()
                .map(|b| (rel.as_str(), b))
        })
        .collect();
    let elsewhere = source("xtask/src/linux/mod.rs") + &source("tools/lunarbar/src/open.rs");
    for name in d.wrappers() {
        if by_path.contains_key(name) {
            continue;
        }
        // Not its own file: nearly every wrapper prints its own name in an
        // error message, and a script that only names itself is not reached.
        let own = format!("usr/local/bin/{name}");
        let bare = bodies
            .iter()
            .filter(|(rel, _)| *rel != own)
            .any(|(_, body)| {
                body.lines()
                    .filter(|l| !l.trim_start().starts_with('#'))
                    .any(|l| l.contains(name))
            });
        assert!(
            bare || elsewhere.contains(name),
            "nothing in the image runs /usr/local/bin/{name}: it is named by no \
             file the desktop writes, by no service in mod.rs and by no row of \
             the panel's opener table"
        );
    }
}

/// The panel's built-in opener table is the only thing that runs
/// `eclipse-edit` and `eclipse-view`; neither is in a menu, a keybind or a
/// `.desktop` entry. The table lives in another crate that is not even in this
/// workspace, so a rename on either side is caught by nothing at all.
#[test]
fn the_openers_table_in_the_panel_names_the_wrappers_the_desktop_writes() {
    let d = installed();
    let table = source("tools/lunarbar/src/open.rs");
    let mine = d.wrappers();
    let mut named = 0;
    for line in table.lines() {
        let Some((_, rest)) = line.split_once("\", \"eclipse-") else {
            continue;
        };
        let wrapper = rest
            .split(|c: char| c == ' ' || c == '"')
            .next()
            .unwrap_or_default();
        let wrapper = format!("eclipse-{wrapper}");
        assert!(
            mine.contains(wrapper.as_str()),
            "the panel opens files with {wrapper}, which the desktop does not \
             install: {}",
            line.trim()
        );
        named += 1;
    }
    assert!(
        named >= 2,
        "the opener table no longer names an eclipse wrapper, or its shape \
         changed and this test reads nothing"
    );
}

/// A launcher reads these five keys and nothing else; `Exec` without `TryExec`
/// is a row that stays in the menu when its program is missing, which is the
/// whole reason both name the wrapper rather than the real binary.
#[test]
fn every_desktop_entry_runs_a_wrapper_and_carries_the_keys_a_launcher_reads() {
    let d = installed();
    let entries: Vec<&String> = d.files.iter().filter(|f| f.ends_with(".desktop")).collect();
    assert!(
        entries.len() >= 3,
        "the install stopped writing its launcher entries: {entries:?}"
    );
    let mine = d.wrappers();
    for rel in entries {
        let text = d.text(rel);
        assert!(
            text.starts_with("[Desktop Entry]\n"),
            "{rel} does not open with its group header"
        );
        let value = |key: &str| -> String {
            text.lines()
                .find_map(|l| l.strip_prefix(&format!("{key}=")))
                .unwrap_or_else(|| panic!("{rel} has no {key}="))
                .to_string()
        };
        assert_eq!(value("Type"), "Application", "{rel}");
        assert!(!value("Name").is_empty(), "{rel} has an empty Name");
        let try_exec = value("TryExec");
        let exec = value("Exec");
        assert!(
            exec.starts_with(&try_exec),
            "{rel}: TryExec is {try_exec} but Exec runs {exec}, so the row is \
             shown for one program and runs another"
        );
        let wrapper = try_exec
            .strip_prefix("/usr/local/bin/")
            .unwrap_or_else(|| panic!("{rel}: TryExec={try_exec} is not a wrapper"));
        assert!(
            mine.contains(wrapper),
            "{rel} runs {wrapper}, which the desktop does not install"
        );
    }
}

// ---------------------------------------------------------------------------
// The wallpaper, Xorg, and the digest
// ---------------------------------------------------------------------------

/// The path is in the module's own documentation as the one thing the
/// wallpaper renderer produces, and in the writer as four path components. Two
/// copies, and the one a reader believes is the doc.
#[test]
fn the_wallpaper_is_written_where_the_module_says_it_is() {
    let doc = source("xtask/src/linux/desktop.rs");
    let advertised = doc
        .lines()
        .take_while(|l| l.starts_with("//!"))
        .find_map(|l| l.trim().split_once("`/usr/share/backgrounds/"))
        .and_then(|(_, rest)| rest.split_once('`'))
        .map(|(path, _)| format!("usr/share/backgrounds/{path}"))
        .expect("the module docs no longer say where the wallpaper goes");

    let dir = std::env::temp_dir().join(format!("eclipse-wallpaper-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_wallpaper(&dir);
    let png = dir.join(&advertised);
    let bytes = fs::read(&png).unwrap_or_else(|e| panic!("{}: {e}", png.display()));
    assert_eq!(
        &bytes[..8],
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        "{advertised} is not a PNG"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The render is the most expensive thing in the rootfs build, so it is
/// skipped when the file is there. What that costs is that a corrupt or
/// truncated wallpaper is never replaced -- stated here so it stays a
/// decision, and so the skip itself cannot be dropped without a test saying
/// the build got slower.
#[test]
fn a_wallpaper_that_is_already_there_is_not_drawn_again() {
    let dir = std::env::temp_dir().join(format!("eclipse-wallpaper-skip-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let png = dir.join("usr/share/backgrounds/eclipse/eclipse-night.png");
    fs::create_dir_all(png.parent().unwrap()).unwrap();
    fs::write(&png, b"not a picture at all").unwrap();
    write_wallpaper(&dir);
    assert_eq!(
        fs::read(&png).unwrap(),
        b"not a picture at all",
        "the wallpaper was redrawn over a file that was already there"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Xorg here runs on the kernel framebuffer. Letting it platform-probe
/// `/dev/dri/card0` hangs the server before the screen comes up, so startx
/// times out and init respawns X every ninety seconds -- the three options
/// below are what stop that, and none of them is obviously load-bearing from
/// the diff.
#[test]
fn xorg_is_pinned_to_the_framebuffer_and_never_probes_drm() {
    let d = installed();
    let conf = d.text("etc/X11/xorg.conf.d/10-eclipse.conf");
    for (option, value) in [
        ("AutoAddGPU", "false"),
        ("AutoBindGPU", "false"),
        ("AutoAddDevices", "true"),
    ] {
        assert!(
            conf.lines().any(|l| {
                let l = l.trim();
                l.starts_with("Option")
                    && l.contains(option)
                    && l.ends_with(&format!("\"{value}\""))
            }),
            "10-eclipse.conf no longer sets {option} to {value}:\n{conf}"
        );
    }
    assert!(
        conf.contains("\"fbdev\""),
        "the Device section no longer names the fbdev driver"
    );
    // In a comment it is the thing this file says it is NOT doing; on a line
    // Xorg reads it would be the DRM path the three options above exist to
    // keep it off.
    assert!(
        !conf
            .lines()
            .any(|l| !l.trim_start().starts_with('#') && l.contains("modesetting")),
        "a modesetting Device is the DRM path this config exists to avoid:\n{conf}"
    );
}

/// busybox ships no `mcookie`, so `startx` cannot mint a magic cookie and the
/// server rejects every client while startx spins. `-ac` is what gets a client
/// connected on the first try, and it has to be on the exec line rather than
/// in a comment.
#[test]
fn the_x_server_is_started_without_the_cookie_startx_cannot_mint() {
    let d = installed();
    let rc = d.text("root/.xserverrc");
    let execs: Vec<&str> = rc
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && l.contains("exec "))
        .collect();
    assert!(!execs.is_empty(), "nothing in .xserverrc execs a server");
    for line in execs {
        assert!(
            line.contains("-ac"),
            ".xserverrc starts a server with access control on: {line}"
        );
        assert!(
            line.contains("-nolisten tcp"),
            ".xserverrc opens a TCP listener on a single-user VM: {line}"
        );
        assert!(
            line.contains("\"$@\""),
            ".xserverrc drops the display and vt arguments startx appends: {line}"
        );
    }
}

/// The pinned Freedoom archive is checked against this, so a digest that comes
/// back wrong -- or `None` because neither tool is installed, which is a pass
/// rather than a failure in the caller -- decides whether a download is
/// trusted.
#[test]
fn a_files_digest_is_the_sha512_the_host_tools_print() {
    let dir = std::env::temp_dir().join(format!("eclipse-sha512-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let empty = dir.join("empty");
    fs::write(&empty, b"").unwrap();
    let abc = dir.join("abc");
    fs::write(&abc, b"abc").unwrap();

    // The two published SHA-512 vectors, lower case as the function returns it.
    let want_empty = "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
                      47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e";
    let want_abc = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
                    2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";

    match sha512_of(&empty) {
        Some(got) => {
            assert_eq!(got, want_empty, "the digest of an empty file is wrong");
            assert_eq!(
                sha512_of(&abc).as_deref(),
                Some(want_abc),
                "the digest of \"abc\" is wrong"
            );
            assert_eq!(
                sha512_of(&dir.join("not-here")),
                None,
                "a file that is not there has no digest"
            );
        }
        // Neither sha512sum nor shasum on this host. The caller treats that as
        // "cannot verify" and goes on, so there is nothing else to check here.
        None => assert_eq!(
            sha512_of(&abc),
            None,
            "one file hashed and the other did not"
        ),
    }
    let _ = fs::remove_dir_all(&dir);
}
