//! Application discovery for lunarbar's launcher menu — a dependency-free
//! reader for the freedesktop.org (XDG) Desktop Entry + Base Directory specs
//! ("OpenDesktop"). Any program that ships a standard `.desktop` file shows up
//! automatically, exactly as it would in a full desktop's menu.
//!
//! Implemented per spec:
//! - directories: `$XDG_DATA_HOME` (default `~/.local/share`) then each of
//!   `$XDG_DATA_DIRS` (default `/usr/local/share:/usr/share`), each + `/applications`,
//!   recursed; earlier dirs win by desktop-file ID (so a user override shadows
//!   the system copy).
//! - only `Type=Application` entries, skipping `NoDisplay=true` / `Hidden=true`.
//! - `OnlyShowIn` / `NotShowIn` honoured against `$XDG_CURRENT_DESKTOP`, which
//!   the packaging sets to `labwc:wlroots` (see `xtask`'s desktop profile), so
//!   an entry gated to either name shows. The aliases come from that variable
//!   and NOT from this file: nothing here adds a name the compositor's
//!   environment did not already claim.
//! - `TryExec` must resolve on `PATH` (or as an absolute path) or the entry is
//!   dropped, so dead menu items never appear.
//! - `Exec` field codes (`%f`, `%U`, …) stripped; `Terminal=true` entries are
//!   wrapped in the eclipse-terminal command.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Hard ceiling on menu entries (and walk budget) so opening the launcher
/// cannot stall the Wayland event loop on a pathological applications tree.
const MAX_APPS: usize = 2_048;
const MAX_WALK: usize = 8_000;
const MAX_DEPTH: u32 = 8;
const MAX_DESKTOP_BYTES: u64 = 256 * 1024;

pub struct AppEntry {
    pub name: String,
    pub exec: String,
    /// `Icon=` value (name or absolute path), resolved by icons::IconCache.
    pub icon: Option<String>,
}

/// Scan the XDG applications directories for launchable entries. `terminal`
/// wraps `Terminal=true` programs (and is the builtin Terminal row's command).
pub fn scan_apps(terminal: &str) -> Vec<AppEntry> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let data_home = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{home}/.local/share"));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());

    let current = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let desktops = desktops_from(&current);
    let lang = std::env::var("LANG").unwrap_or_default();
    let lang = locale_language(&lang);

    let roots = xdg_roots(&data_home, &data_dirs);

    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut out: Vec<AppEntry> = Vec::new();
    let mut budget = MAX_WALK;
    for root in roots {
        if out.len() >= MAX_APPS || budget == 0 {
            break;
        }
        let apps_dir = root.join("applications");
        collect_dir(
            &apps_dir,
            terminal,
            &desktops,
            lang.as_deref(),
            &mut seen_ids,
            &mut out,
            &mut budget,
        );
    }
    sort_menu(&mut out);
    out
}

/// Put the menu in the order the launcher shows it.
///
/// By the SAME normalisation the filter uses ([`norm_key`]). It used to sort by
/// `to_lowercase`, which compares raw code points: 'a' is 0x61 and 'a' with an
/// acute is 0xE1, past 'z', so every app whose name starts with an accented
/// letter landed after "Zebra" -- "Abaco", "Album" and "Exito" all at the bottom
/// of a Spanish menu -- while typing "abaco" found it, because the filter had
/// already normalised. One normalisation for both, so where an entry sorts and
/// what finds it agree.
///
/// A separate function because that is the only way a test can reach the
/// comparison: `scan_apps` needs a filesystem and three environment variables.
pub fn sort_menu(out: &mut [AppEntry]) {
    out.sort_by_key(|a| norm_key(&a.name));
}

/// The desktop names this session answers to, from `$XDG_CURRENT_DESKTOP`.
///
/// Lowercased because `OnlyShowIn`/`NotShowIn` are compared case-insensitively.
/// An empty variable gives an empty list, which per the spec hides every entry
/// carrying an `OnlyShowIn` -- the desktop is unknown, so no gate can pass.
pub fn desktops_from(current: &str) -> Vec<String> {
    current
        .split(':')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// The XDG applications roots, in priority order: `$XDG_DATA_HOME` first (so a
/// user override shadows the system copy by desktop-file ID), then each of
/// `$XDG_DATA_DIRS`. The caller has already applied the spec's defaults.
pub fn xdg_roots(data_home: &str, data_dirs: &str) -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from(data_home)];
    roots.extend(
        data_dirs
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    );
    roots
}

/// The language part of a POSIX locale: `es_ES.UTF-8` -> `es`. `None` for an
/// empty or `C`/`POSIX` locale, where no `Name[xx]` key should win.
pub fn locale_language(lang: &str) -> Option<String> {
    // The charset and the modifier go first: a locale with NO region
    // ("es.UTF-8") would otherwise keep them, because there is no '_' to cut at.
    let lang = lang.split(['.', '@']).next().unwrap_or("").trim();
    let lang = lang.split('_').next().unwrap_or("");
    if lang.is_empty() || lang == "C" || lang == "POSIX" {
        return None;
    }
    Some(lang.to_ascii_lowercase())
}

/// Walk `root`, deriving each file's desktop-file ID from its path relative to
/// `root` (subdir separators become '-', per spec). Iterative instead of
/// recursive to avoid deep coroutine call stacks on large application trees.
#[allow(clippy::too_many_arguments)]
fn collect_dir(
    root: &Path,
    terminal: &str,
    desktops: &[String],
    lang: Option<&str>,
    seen: &mut HashSet<String>,
    out: &mut Vec<AppEntry>,
    budget: &mut usize,
) {
    // Stack items: (directory, depth).
    let mut stack: Vec<(PathBuf, u32)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH || *budget == 0 || out.len() >= MAX_APPS {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            if *budget == 0 || out.len() >= MAX_APPS {
                return;
            }
            *budget = budget.saturating_sub(1);
            let p = e.path();
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push((p, depth + 1));
            } else if p.extension().map(|x| x == "desktop").unwrap_or(false) {
                if !seen.insert(desktop_id(root, &p)) {
                    continue; // shadowed by a higher-priority dir
                }
                if let Some(a) = parse_desktop(&p, terminal, desktops, lang) {
                    out.push(a);
                }
            }
        }
    }
}

/// Parse one `.desktop` file into a menu entry, or None if the spec says it
/// should not be shown here.
fn parse_desktop(
    path: &Path,
    terminal: &str,
    desktops: &[String],
    lang: Option<&str>,
) -> Option<AppEntry> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_DESKTOP_BYTES {
        return None;
    }
    let s = std::fs::read_to_string(path).ok()?;
    parse_desktop_text(&s, terminal, desktops, lang, executable_exists)
}

/// The desktop-file ID: the path relative to its root with `/` turned into `-`,
/// per the spec. It is what makes a user's copy shadow the system one, so it is
/// derived in exactly one place.
///
/// A file outside `root` (a symlink walked out of the tree) keeps its whole
/// path as the ID rather than being dropped: two different files must not
/// collide, and an ID nobody can construct on purpose is better than an entry
/// that silently shadows an unrelated one.
pub fn desktop_id(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('/', "-")
}

/// The pure half of [`parse_desktop`]: everything the spec decides, given the
/// file's text. `exec_exists` answers `TryExec`, injected so this can be tested
/// without a filesystem.
pub fn parse_desktop_text(
    text: &str,
    terminal: &str,
    desktops: &[String],
    lang: Option<&str>,
    exec_exists: impl Fn(&str) -> bool,
) -> Option<AppEntry> {
    let mut in_entry = false;
    let (mut name, mut name_loc, mut exec, mut try_exec) = (None, None, None, None);
    let mut icon = None;
    let (mut is_app, mut hidden, mut wants_term) = (false, false, false);
    let (mut only_show, mut not_show) = (Vec::new(), Vec::new());

    // How specific the localized name already found is: 2 for an exact
    // `lang_REGION`, 1 for a bare `lang`. Without this the FIRST matching key in
    // the file won, so `Name[es_AR]` before `Name[es]` and the same file the
    // other way round gave different menus.
    let mut name_loc_rank = 0u8;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]"; // ignore actions/other groups
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "Name" => name = Some(v.to_string()),
            "Type" => is_app = v == "Application",
            "Exec" => exec = Some(v.to_string()),
            "Icon" if !v.is_empty() => icon = Some(v.to_string()),
            "TryExec" => try_exec = Some(v.to_string()),
            "NoDisplay" | "Hidden" if v == "true" => hidden = true,
            "Terminal" if v == "true" => wants_term = true,
            "OnlyShowIn" => only_show = split_list(v),
            "NotShowIn" => not_show = split_list(v),
            _ => {
                // Localized name for our locale's language, e.g. Name[es].
                if let Some(rank) = locale_rank(k, lang) {
                    // Strictly greater: two keys of the same specificity
                    // ("Name[es_ES]" and "Name[es_MX]") leave the first one, so
                    // the answer does not depend on which came last.
                    if rank > name_loc_rank {
                        name_loc_rank = rank;
                        name_loc = Some(v.to_string());
                    }
                }
            }
        }
    }

    if !is_app || hidden {
        return None;
    }
    // OnlyShowIn / NotShowIn gating against the current desktop(s).
    if !only_show.is_empty() && !only_show.iter().any(|d| desktops.contains(d)) {
        return None;
    }
    if not_show.iter().any(|d| desktops.contains(d)) {
        return None;
    }
    // TryExec: the named binary must exist, else the entry is a dead link.
    if let Some(te) = try_exec {
        if !exec_exists(&te) {
            return None;
        }
    }

    let name = name_loc.or(name)?;
    let exec = strip_field_codes(&exec?);
    if exec.is_empty() {
        return None;
    }
    let exec = if wants_term {
        format!("{terminal} {exec}")
    } else {
        exec
    };
    Some(AppEntry { name, exec, icon })
}

/// `OnlyShowIn`/`NotShowIn` are ';'-separated, lowercased for comparison.
fn split_list(v: &str) -> Vec<String> {
    v.split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// How well a `Name[xx]` / `Name[xx_YY]` key matches `lang`: 2 for a key that
/// names a region (`Name[es_ES]`), 1 for the bare language (`Name[es]`), `None`
/// for anything else.
///
/// The rank is what makes the choice independent of the order the keys appear
/// in, and `lang` is a parameter rather than a read of `$LANG` so the decision
/// can be tested and so the variable is read once per scan instead of once per
/// unmatched key of every file.
pub fn locale_rank(key: &str, lang: Option<&str>) -> Option<u8> {
    let lang = lang?;
    let tag = key.strip_prefix("Name[")?.strip_suffix(']')?;
    let tag = tag.split('.').next().unwrap_or(tag);
    let mut parts = tag.split(['_', '-']);
    if !parts.next()?.eq_ignore_ascii_case(lang) {
        return None;
    }
    Some(if parts.next().is_some() { 2 } else { 1 })
}

/// Remove XDG Exec field codes (`%f %F %u %U %i %c %k` …); collapse whitespace.
///
/// One left-to-right pass, because replacing the codes one at a time got `%%`
/// wrong: the spec says `%%` is a LITERAL percent, and `out.replace("%f", "")`
/// saw the `%f` inside `%%f` and left a bare `%` where `%f` belonged. A percent
/// followed by anything that is not a field code is left alone, so a command
/// that legitimately contains one (`sh -c 'printf %s'`) survives.
pub fn strip_field_codes(exec: &str) -> String {
    const CODES: &str = "UuFficnkdDNvm";
    let mut out = String::with_capacity(exec.len());
    let mut it = exec.chars();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('%') => out.push('%'),
            Some(code) if CODES.contains(code) => {} // a field code: dropped
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            // A trailing '%' is not a code; keep it rather than eat it.
            None => out.push('%'),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Search-normalise a string: lowercase with Latin-1 diacritics stripped, so
/// typing "configuracion" in the launcher filter matches "Configuración".
pub fn norm_key(s: &str) -> String {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' | 'ã' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' | 'õ' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            c => c,
        })
        .collect()
}

/// Where to look for a bare command name when `$PATH` is unset.
///
/// **One list for the whole crate.** This file used to use
/// `/usr/local/bin:/usr/bin:/bin` while `lunarrun` used this one, so a
/// `TryExec=` naming a binary in `/sbin` or `/usr/sbin` -- which on a busybox
/// image is a great many of them -- made the menu entry DISAPPEAR while the
/// runner would have launched it happily.
pub const DEFAULT_PATH: &str = "/usr/local/bin:/bin:/usr/bin:/sbin:/usr/sbin";

/// True if `cmd` (absolute, or a bare name on `PATH`) is an executable file.
pub fn executable_exists(cmd: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let is_exec = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if cmd.contains('/') {
        return is_exec(Path::new(cmd));
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| DEFAULT_PATH.into());
    path.split(':')
        .filter(|d| !d.is_empty())
        .any(|d| is_exec(&Path::new(d).join(cmd)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `.desktop` body with the minimum the spec needs to show an entry.
    fn entry(extra: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName=Thing\nExec=thing\n{extra}")
    }

    fn parse(text: &str) -> Option<AppEntry> {
        parse_desktop_text(
            text,
            "term -e",
            &["labwc".into(), "wlroots".into()],
            Some("es"),
            |_| true,
        )
    }

    fn menu(names: &[&str]) -> Vec<String> {
        let mut v: Vec<AppEntry> = names
            .iter()
            .map(|n| AppEntry {
                name: (*n).to_string(),
                exec: "x".into(),
                icon: None,
            })
            .collect();
        sort_menu(&mut v);
        v.into_iter().map(|a| a.name).collect()
    }

    #[test]
    fn the_menu_itself_is_ordered_by_the_key_the_filter_uses() {
        // Through `sort_menu`, so the comparison the launcher actually runs is
        // the one under test and not a restatement of it.
        assert_eq!(
            menu(&[
                "Zebra",
                "Ábaco",
                "Configuración",
                "Álbum",
                "Firefox",
                "Éxito"
            ]),
            [
                "Ábaco",
                "Álbum",
                "Configuración",
                "Éxito",
                "Firefox",
                "Zebra"
            ]
        );
        // Case does not decide the order either.
        assert_eq!(
            menu(&["banana", "Apple", "cherry"]),
            ["Apple", "banana", "cherry"]
        );
        assert!(menu(&[]).is_empty());
        assert_eq!(menu(&["Solo"]), ["Solo"]);
    }

    #[test]
    fn an_accented_name_sorts_where_someone_would_look_for_it() {
        // The regression: the menu sorted by `to_lowercase`, which compares raw
        // code points ('a' is 0x61, 'a'-acute is 0xE1, past 'z'), so every app
        // whose name starts with an accented letter landed after "Zebra" --
        // while typing "abaco" found it, because the FILTER already normalised.
        let names = [
            "Zebra",
            "Ábaco",
            "Configuración",
            "Álbum",
            "Firefox",
            "Éxito",
        ];
        let mut by_norm: Vec<&str> = names.to_vec();
        by_norm.sort_by_key(|n| norm_key(n));
        assert_eq!(
            by_norm,
            [
                "Ábaco",
                "Álbum",
                "Configuración",
                "Éxito",
                "Firefox",
                "Zebra"
            ]
        );
        // And the old key really did put them at the bottom, so this test is
        // pinning a fix and not a coincidence.
        let mut by_lower: Vec<&str> = names.to_vec();
        by_lower.sort_by_key(|n| n.to_lowercase());
        assert_eq!(by_lower.last(), Some(&"Éxito"));
        assert_ne!(by_lower, by_norm);
    }

    #[test]
    fn the_search_normalisation_strips_the_accents_a_spanish_menu_has() {
        assert_eq!(norm_key("Configuración"), "configuracion");
        assert_eq!(norm_key("ÁÉÍÓÚ"), "aeiou");
        assert_eq!(norm_key("Español Ñandú"), "espanol nandu");
        assert_eq!(norm_key("Français Çà"), "francais ca");
        assert_eq!(norm_key("Über"), "uber");
        // Anything it has no rule for passes through lowercased, rather than
        // being dropped: a name in another script must still be searchable.
        assert_eq!(norm_key("Ωmega"), "ωmega");
        assert_eq!(norm_key(""), "");
    }

    #[test]
    fn the_desktops_this_session_answers_to_come_only_from_the_environment() {
        // The packaging sets XDG_CURRENT_DESKTOP=labwc:wlroots; both names have
        // to come out, because entries in the wild gate on either.
        assert_eq!(desktops_from("labwc:wlroots"), ["labwc", "wlroots"]);
        assert_eq!(desktops_from("KDE"), ["kde"], "compared lowercased");
        assert_eq!(desktops_from(" labwc : wlroots "), ["labwc", "wlroots"]);
        assert_eq!(desktops_from("a::b"), ["a", "b"], "empty segments dropped");
        // Unknown desktop: the list is empty, which is what hides every entry
        // carrying an OnlyShowIn, per the spec.
        assert!(desktops_from("").is_empty());
        assert!(desktops_from(":::").is_empty());
    }

    #[test]
    fn the_user_directory_comes_before_the_system_ones() {
        // The order is what makes a user's copy shadow the system one by ID.
        let roots = xdg_roots("/home/u/.local/share", "/usr/local/share:/usr/share");
        assert_eq!(
            roots,
            [
                PathBuf::from("/home/u/.local/share"),
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ]
        );
        // An empty XDG_DATA_DIRS leaves just the home root, not a root of "".
        assert_eq!(xdg_roots("/h", ""), [PathBuf::from("/h")]);
        assert_eq!(xdg_roots("/h", "/a::/b").len(), 3);
    }

    #[test]
    fn the_locale_language_is_the_part_before_the_region_and_the_charset() {
        assert_eq!(locale_language("es_ES.UTF-8").as_deref(), Some("es"));
        assert_eq!(locale_language("es").as_deref(), Some("es"));
        assert_eq!(locale_language("en_US.iso88591").as_deref(), Some("en"));
        assert_eq!(locale_language("ca_ES@valencia").as_deref(), Some("ca"));
        // No region at all: the charset still has to come off, and there is no
        // '_' to cut at.
        assert_eq!(locale_language("es.UTF-8").as_deref(), Some("es"));
        assert_eq!(locale_language("de@euro").as_deref(), Some("de"));
        assert_eq!(locale_language(" es_ES ").as_deref(), Some("es"));
        // `C` and `POSIX` mean "no localization", so no Name[xx] should win.
        assert_eq!(locale_language("C"), None);
        assert_eq!(locale_language("POSIX"), None);
        assert_eq!(locale_language(""), None);
    }

    #[test]
    fn the_most_specific_localized_name_wins_whatever_order_it_appears_in() {
        // The old code took the FIRST matching key, so the same file with its
        // two Name keys swapped gave a different menu entry.
        let a = entry("Name[es]=Generico\nName[es_ES]=Especifico\n");
        let b = entry("Name[es_ES]=Especifico\nName[es]=Generico\n");
        assert_eq!(parse(&a).unwrap().name, "Especifico");
        assert_eq!(parse(&b).unwrap().name, "Especifico");
        // And the ranking itself.
        assert_eq!(locale_rank("Name[es]", Some("es")), Some(1));
        assert_eq!(locale_rank("Name[es_ES]", Some("es")), Some(2));
        assert_eq!(locale_rank("Name[es-ES]", Some("es")), Some(2));
        assert_eq!(locale_rank("Name[ES]", Some("es")), Some(1), "case");
        assert_eq!(locale_rank("Name[en]", Some("es")), None);
        assert_eq!(locale_rank("Name[es]", None), None, "no locale set");
        // Two keys of the SAME specificity: the first in the file wins, so the
        // menu does not change because someone reordered two lines.
        let both = entry("Name[es_ES]=Primero\nName[es_MX]=Segundo\n");
        assert_eq!(parse(&both).unwrap().name, "Primero");
        let swapped = entry("Name[es_MX]=Segundo\nName[es_ES]=Primero\n");
        assert_eq!(parse(&swapped).unwrap().name, "Segundo");
        // Not a localized Name key at all.
        assert_eq!(locale_rank("Comment[es]", Some("es")), None);
        assert_eq!(locale_rank("Name", Some("es")), None);
        assert_eq!(locale_rank("Name[es", Some("es")), None);
    }

    #[test]
    fn a_localized_name_is_used_and_the_plain_one_is_the_fallback() {
        assert_eq!(parse(&entry("Name[es]=Cosa\n")).unwrap().name, "Cosa");
        assert_eq!(parse(&entry("Name[en]=Widget\n")).unwrap().name, "Thing");
        let en = parse_desktop_text(
            &entry("Name[es]=Cosa\nName[en]=Widget\n"),
            "term -e",
            &[],
            Some("en"),
            |_| true,
        );
        assert_eq!(en.unwrap().name, "Widget");
    }

    #[test]
    fn the_entry_id_is_the_path_under_its_root_with_the_separators_flattened() {
        // It is what makes a user's copy shadow the system one, so two files at
        // the same relative path must give the same ID and two different files
        // must not.
        let root = Path::new("/usr/share/applications");
        assert_eq!(
            desktop_id(root, Path::new("/usr/share/applications/foo.desktop")),
            "foo.desktop"
        );
        assert_eq!(
            desktop_id(root, Path::new("/usr/share/applications/kde/bar.desktop")),
            "kde-bar.desktop"
        );
        assert_eq!(
            desktop_id(
                Path::new("/home/u/.local/share/applications"),
                Path::new("/home/u/.local/share/applications/kde/bar.desktop")
            ),
            "kde-bar.desktop",
            "the same relative path in another root is the same ID"
        );
        // A path outside the root keeps its whole path rather than collapsing
        // onto some other entry's ID.
        assert_eq!(
            desktop_id(root, Path::new("/opt/x.desktop")),
            "-opt-x.desktop"
        );
    }

    #[test]
    fn a_double_percent_is_a_literal_percent_and_not_half_a_field_code() {
        // Replacing the codes one at a time saw the `%f` inside `%%f`.
        assert_eq!(strip_field_codes("foo %%f"), "foo %f");
        assert_eq!(strip_field_codes("foo %%"), "foo %");
        assert_eq!(strip_field_codes("100%% done"), "100% done");
        // A trailing percent is not a code; do not eat it.
        assert_eq!(strip_field_codes("foo %"), "foo %");
        // A percent followed by something that is not a field code survives,
        // so a command that legitimately contains one still runs.
        assert_eq!(strip_field_codes("sh -c printf %s"), "sh -c printf %s");
    }

    #[test]
    fn every_field_code_the_spec_defines_is_removed() {
        for code in [
            "%U", "%u", "%F", "%f", "%i", "%c", "%k", "%d", "%D", "%n", "%N", "%v", "%m",
        ] {
            assert_eq!(
                strip_field_codes(&format!("app {code}")),
                "app",
                "{code} survived"
            );
            assert_eq!(
                strip_field_codes(&format!("app {code} --flag")),
                "app --flag"
            );
        }
        // And the whitespace the removal leaves behind is collapsed, so the
        // command does not end up with a double space or a trailing one.
        assert_eq!(strip_field_codes("app  %f   --flag  "), "app --flag");
        assert_eq!(strip_field_codes("%f"), "");
    }

    #[test]
    fn only_an_application_that_wants_to_be_seen_becomes_a_menu_entry() {
        assert!(parse(&entry("")).is_some());
        // Not an application.
        assert!(parse("[Desktop Entry]\nType=Link\nName=T\nExec=t\n").is_none());
        assert!(
            parse("[Desktop Entry]\nName=T\nExec=t\n").is_none(),
            "no Type"
        );
        // Asked not to be shown.
        assert!(parse(&entry("NoDisplay=true\n")).is_none());
        assert!(parse(&entry("Hidden=true\n")).is_none());
        // `false` is not `true`: the entry stays.
        assert!(parse(&entry("NoDisplay=false\n")).is_some());
        // No name, or no command to run.
        assert!(parse("[Desktop Entry]\nType=Application\nExec=t\n").is_none());
        assert!(parse("[Desktop Entry]\nType=Application\nName=T\n").is_none());
        // An Exec that is nothing but field codes leaves no command at all.
        assert!(parse("[Desktop Entry]\nType=Application\nName=T\nExec=%f %U\n").is_none());
    }

    #[test]
    fn the_desktop_gates_are_honoured_in_both_directions() {
        assert!(parse(&entry("OnlyShowIn=labwc;\n")).is_some());
        assert!(parse(&entry("OnlyShowIn=wlroots;GNOME;\n")).is_some());
        assert!(parse(&entry("OnlyShowIn=LABWC;\n")).is_some(), "case");
        assert!(parse(&entry("OnlyShowIn=GNOME;\n")).is_none());
        assert!(parse(&entry("NotShowIn=labwc;\n")).is_none());
        assert!(parse(&entry("NotShowIn=GNOME;\n")).is_some());
        // With an unknown desktop, an OnlyShowIn cannot pass and a NotShowIn
        // cannot match -- which is the spec's answer, not an accident.
        let unknown = |extra: &str| {
            parse_desktop_text(&entry(extra), "term -e", &[], Some("es"), |_| true).is_some()
        };
        assert!(!unknown("OnlyShowIn=labwc;\n"));
        assert!(unknown("NotShowIn=labwc;\n"));
        assert!(unknown(""));
    }

    #[test]
    fn an_entry_whose_tryexec_is_not_installed_never_reaches_the_menu() {
        // A dead menu row is worse than a missing one: it looks like the app is
        // there and does nothing when clicked.
        let missing = |text: &str| parse_desktop_text(text, "term -e", &[], Some("es"), |_| false);
        assert!(missing(&entry("TryExec=ghost\n")).is_none());
        // No TryExec at all: the probe is never consulted.
        assert!(missing(&entry("")).is_some());
        // And the probe is handed the TryExec value, not the Exec line.
        let seen = std::cell::RefCell::new(Vec::new());
        let _ = parse_desktop_text(
            &entry("TryExec=/opt/bin/thing\n"),
            "term -e",
            &[],
            Some("es"),
            |c| {
                seen.borrow_mut().push(c.to_string());
                true
            },
        );
        assert_eq!(seen.into_inner(), ["/opt/bin/thing"]);
    }

    #[test]
    fn a_terminal_program_is_wrapped_and_a_graphical_one_is_not() {
        assert_eq!(parse(&entry("")).unwrap().exec, "thing");
        assert_eq!(
            parse(&entry("Terminal=true\n")).unwrap().exec,
            "term -e thing"
        );
        assert_eq!(parse(&entry("Terminal=false\n")).unwrap().exec, "thing");
    }

    #[test]
    fn only_the_desktop_entry_group_is_read() {
        // Actions and other groups carry their own Name and Exec; reading them
        // would put an action's command on the app's row.
        let with_action = "[Desktop Entry]\nType=Application\nName=Real\nExec=real\n\
                           [Desktop Action new]\nName=Wrong\nExec=wrong\n";
        let a = parse(with_action).unwrap();
        assert_eq!(a.name, "Real");
        assert_eq!(a.exec, "real");
        // Keys before any group header are not in [Desktop Entry] either.
        assert!(parse("Type=Application\nName=T\nExec=t\n").is_none());
        // Space around the equals sign is ignored, per the spec: without
        // trimming, "Name = T" leaves the key as "Name " and the entry has no
        // name at all, so it silently never appears.
        let spaced = "[Desktop Entry]\nType = Application\nName = Spaced\nExec = spaced -v\n";
        let a = parse(spaced).unwrap();
        assert_eq!(a.name, "Spaced");
        assert_eq!(a.exec, "spaced -v");
        // Comments and blank lines are skipped, and a line with no '=' is not a
        // key (a stray word must not become an empty Name).
        let messy = "# a comment\n\n[Desktop Entry]\n# another\nType=Application\n\
                     nonsense\nName=T\nExec=t\n";
        assert_eq!(parse(messy).unwrap().name, "T");
    }

    #[test]
    fn an_icon_is_kept_when_it_names_something() {
        assert_eq!(
            parse(&entry("Icon=firefox\n")).unwrap().icon.as_deref(),
            Some("firefox")
        );
        assert_eq!(
            parse(&entry("Icon=/usr/share/pixmaps/a.png\n"))
                .unwrap()
                .icon
                .as_deref(),
            Some("/usr/share/pixmaps/a.png")
        );
        // An empty Icon= is no icon, not an icon named "".
        assert_eq!(parse(&entry("Icon=\n")).unwrap().icon, None);
        assert_eq!(parse(&entry("")).unwrap().icon, None);
    }

    #[test]
    fn the_fallback_search_path_covers_where_this_image_keeps_its_binaries() {
        // The bug this const exists to prevent: this file used to omit /sbin
        // and /usr/sbin while `lunarrun` included them, so a TryExec naming a
        // binary there made the menu entry disappear while the runner would
        // have launched it.
        for dir in ["/usr/local/bin", "/bin", "/usr/bin", "/sbin", "/usr/sbin"] {
            assert!(
                DEFAULT_PATH.split(':').any(|d| d == dir),
                "{dir} is missing from DEFAULT_PATH"
            );
        }
        assert!(!DEFAULT_PATH.split(':').any(str::is_empty));
    }

    #[test]
    fn the_menu_ceilings_are_big_enough_to_be_a_ceiling_and_not_a_limit() {
        // They exist so a pathological applications tree cannot stall the
        // Wayland event loop; a real image has tens of entries, so if any of
        // these is ever reached it is a runaway and not a busy desktop.
        const {
            assert!(MAX_APPS >= 1024);
            assert!(
                MAX_WALK >= MAX_APPS,
                "the walk budget must outlast the menu"
            );
            assert!(MAX_DEPTH >= 4);
            assert!(MAX_DESKTOP_BYTES >= 64 * 1024);
        }
    }
}
