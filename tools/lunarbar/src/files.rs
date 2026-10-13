//! The file browser's logic: paths, directory listings, filtering and the
//! quoting that keeps a filename out of `/bin/sh`'s hands.
//!
//! `lunarfile` is the only caller, but none of this is drawing or Wayland, so
//! it lives here where it can be tested without a compositor — the same split
//! `lunarbar` and `lunarrun` already use for `apps` and `sysinfo`.
//!
//! One rule runs through the whole module: **a filename is bytes the user
//! chose, not a command**. Eclipse's launcher path is `proc::spawn_detached`,
//! which is `/bin/sh -c`, so every path this module hands to a command goes
//! through `shell_quote` first. A file called `a';reboot;'.txt` is a perfectly
//! legal name, and on this system it is also a reboot.

use std::path::Path;

/// One row of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    /// Size in bytes; meaningless for a directory, and shown as `--`.
    pub size: u64,
    /// Whether the name is a symlink, whatever it points at. `is_dir` follows
    /// the link (a link to a directory opens like one), so this is the only
    /// thing that distinguishes the two.
    pub link: bool,
}

/// How many characters of a name are ever kept. A directory entry can be 255
/// bytes and a crafted one much longer once a path is joined; nothing wider
/// than a panel is drawable, and an unbounded String per row is an unbounded
/// allocation driven by whatever is on disk.
pub const MAX_NAME_CHARS: usize = 256;

/// Is `name` hidden by the dotfile convention?
///
/// `.` and `..` are NOT hidden files — they are not entries at all, and
/// `read_dir` never yields them. Anything else starting with a dot is.
pub fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// A directory path in the one shape the rest of the module expects: absolute,
/// no `.` or `..` left in it, and no trailing slash unless it IS the root.
///
/// Done on the string rather than with `canonicalize` on purpose: the browser
/// has to be able to show and leave a directory whose parent is a symlink
/// without the path jumping somewhere else under the user, and it has to give
/// an answer for a path that does not exist (a `--dir` that was deleted) rather
/// than an error. `..` is resolved lexically, and at the root it is the root:
/// `/..` is `/`, which is what the kernel says too.
pub fn normalize_dir(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            p => out.push(p),
        }
    }
    if out.is_empty() {
        return "/".into();
    }
    let mut s = String::with_capacity(path.len() + 1);
    for p in out {
        s.push('/');
        s.push_str(p);
    }
    s
}

/// The directory above `dir`, or None when `dir` is already the root.
///
/// None is what stops Backspace and the `..` row at `/`: a parent-of-root that
/// answered `/` would leave a `..` row that goes nowhere, and one that answered
/// `""` would ask `read_dir` for the current directory, which for a client
/// started by a keybind is not where the user is looking.
pub fn parent_of(dir: &str) -> Option<String> {
    let dir = normalize_dir(dir);
    if dir == "/" {
        return None;
    }
    let cut = dir.rfind('/')?;
    Some(if cut == 0 {
        "/".into()
    } else {
        dir[..cut].into()
    })
}

/// `dir` + `name` as one absolute path.
///
/// `name` comes from `read_dir`, so it is a single component; a `/` in it would
/// be a kernel bug, but joining with a `format!` would then build a path that
/// is not the entry that was clicked. Normalising afterwards costs one pass and
/// makes the result always the shape the next `read_dir` needs.
pub fn join_child(dir: &str, name: &str) -> String {
    normalize_dir(&format!("{dir}/{name}"))
}

/// Order a listing the way a file manager does: directories first, then files,
/// each group by name with case ignored, and ties broken by the name itself so
/// the order is total (`Makefile` and `makefile` are two entries, and a sort
/// that called them equal would let them swap between two listings of the same
/// unchanged directory).
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// The indices of the entries to show, in order: hidden ones only when
/// `show_hidden`, and the rest narrowed by `filter`.
///
/// The match is the panel's own: `apps::norm_key` folds case and strips accents,
/// and a substring anywhere counts, so `pdf` finds `Informe final.PDF`. An empty
/// filter matches everything rather than nothing.
///
/// The filter is trimmed at the ends first: a search box is typed into, and a
/// trailing space is a keystroke on the way to the next letter, not a demand
/// that every name end in a space. The spaces INSIDE it are kept, because
/// `mis cosas` is a name.
pub fn visible(entries: &[Entry], filter: &str, show_hidden: bool) -> Vec<usize> {
    let key = crate::apps::norm_key(filter.trim());
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| show_hidden || !is_hidden(&e.name))
        .filter(|(_, e)| key.is_empty() || crate::apps::norm_key(&e.name).contains(&key))
        .map(|(i, _)| i)
        .collect()
}

/// Read `dir` into a sorted listing.
///
/// Entries whose metadata cannot be read are kept, not dropped: a dangling
/// symlink, or a file in a directory the user can list but not stat, is still
/// something to show, and a listing that silently omits what it could not stat
/// is a file manager that loses files. They come back as 0-byte
/// non-directories, which is what they look like from the outside.
///
/// The stat FOLLOWS symlinks, and `std::fs::metadata` is used rather than
/// `DirEntry::metadata` for exactly that reason: on Unix the latter is an
/// `lstat`, so a symlink to a directory would come back `is_dir: false` and
/// refuse to open, and a dangling one would report the length of its target
/// string as a file size (ten bytes for `/no/existe`). `symlink_metadata` is
/// what says it IS a link, which is only a mark on the row.
pub fn read_dir_sorted(dir: &str) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(Path::new(dir))? {
        let Ok(item) = item else { continue };
        // A name that is not UTF-8 is SKIPPED, not shown lossily. A Unix
        // filename is bytes, and `to_string_lossy` would replace the bad ones
        // with U+FFFD: the row would then name a file that does not exist
        // (nothing could open it), and two different byte names could collapse
        // into the same row -- so activating one would answer with the path of
        // the other. Everything downstream of here is `String`, including the
        // `--out` protocol and the portal's URIs, so showing such a name
        // honestly would mean carrying bytes end to end. Until that is worth
        // doing, a file left out is better than a file confused with another.
        let Some(name) = item.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let md = std::fs::metadata(item.path()).ok();
        let link = item.file_type().map(|t| t.is_symlink()).unwrap_or(false);
        out.push(Entry {
            name: crate::fill_guard::truncate_chars(&name, MAX_NAME_CHARS),
            is_dir: md.as_ref().map(|m| m.is_dir()).unwrap_or(false),
            size: md.as_ref().map(|m| m.len()).unwrap_or(0),
            link,
        });
    }
    sort_entries(&mut out);
    Ok(out)
}

/// A byte count as a file manager shows it: `0 B`, `12 B`, `4,0 K`, `1,5 M`.
///
/// Powers of 1024, one decimal below 10 of a unit and none above, which is what
/// keeps the column from jittering between `9,9 M` and `10,0 M`. The separator
/// is a comma because the image's locale is Spanish and this is the only number
/// in the UI with a fraction in it.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0usize;
    // `UNITS.len() - 1` and not a bare `<`: at u64::MAX the loop would otherwise
    // run one past the table and index out of bounds.
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if v < 10.0 {
        format!("{:.1} {}", v, UNITS[u]).replace('.', ",")
    } else {
        format!("{:.0} {}", v, UNITS[u])
    }
}

/// `s` shortened to `max` characters by cutting the MIDDLE out, with `…`
/// standing in for what went.
///
/// Paths are shortened from the middle and not the end because both ends carry
/// the information: `/home/moebius/…/informe.pdf` says where and what, while the
/// first `max` characters of it say neither. Counted in characters, not bytes,
/// so a path with accents in it does not come back cut through a code point.
pub fn elide_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    // Below three characters there is no room for the ellipsis plus a character
    // either side, so fall back to a plain head cut.
    if max < 3 {
        return s.chars().take(max).collect();
    }
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let chars: Vec<char> = s.chars().collect();
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[n - tail..]);
    out
}

/// `s` as one `/bin/sh` word: wrapped in single quotes, with every embedded
/// single quote closed, escaped and reopened (`'\''`).
///
/// This is the only thing between a filename and `sh -c`. `proc::spawn_detached`
/// runs its argument through the shell, so a file innocently named
/// `No sé qué' ; reboot ; '.txt` is a reboot the moment it is opened, and names
/// with a space, a `$`, a `&` or a newline in them are ordinary. Single quotes
/// are the only shell quoting with no escapes inside them at all, which is why
/// everything goes through them and nothing is special-cased.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Where the browser opens when nobody said: `$HOME`, or `/` when there is no
/// usable one.
///
/// A relative or empty `$HOME` falls through to `/` rather than being joined
/// onto whatever the working directory happens to be: a client started from a
/// keybind inherits the compositor's cwd, and opening there looks like a bug.
pub fn default_dir(home: Option<&str>) -> String {
    match home {
        Some(h) if h.starts_with('/') => normalize_dir(h),
        _ => "/".into(),
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> Entry {
        Entry {
            name: name.into(),
            is_dir: true,
            size: 0,
            link: false,
        }
    }

    fn file(name: &str, size: u64) -> Entry {
        Entry {
            name: name.into(),
            is_dir: false,
            size,
            link: false,
        }
    }

    // ── paths ────────────────────────────────────────────────────────────────

    #[test]
    fn a_path_comes_out_absolute_with_no_trailing_slash_unless_it_is_the_root() {
        // Every other function keys on this shape: two paths for the same
        // directory ("/home/m" and "/home/m/") would make the browser's "am I
        // already here?" check miss and reload the listing under the cursor.
        assert_eq!(normalize_dir("/home/moebius"), "/home/moebius");
        assert_eq!(normalize_dir("/home/moebius/"), "/home/moebius");
        assert_eq!(normalize_dir("/home//moebius///"), "/home/moebius");
        assert_eq!(normalize_dir("/"), "/");
        assert_eq!(normalize_dir("//"), "/");
        assert_eq!(normalize_dir(""), "/");
        // A relative path is made absolute rather than being joined onto a
        // working directory the user never chose.
        assert_eq!(normalize_dir("home/moebius"), "/home/moebius");
    }

    #[test]
    fn dot_and_dotdot_are_resolved_and_dotdot_at_the_root_is_the_root() {
        assert_eq!(normalize_dir("/home/./moebius"), "/home/moebius");
        assert_eq!(normalize_dir("/home/moebius/.."), "/home");
        assert_eq!(normalize_dir("/home/moebius/../otro"), "/home/otro");
        // The kernel answers `/` for `/..`, and so must this: climbing past the
        // root with enough `..` must not produce an empty path, which would
        // make `read_dir` read the working directory instead.
        assert_eq!(normalize_dir("/.."), "/");
        assert_eq!(normalize_dir("/../../../.."), "/");
        assert_eq!(normalize_dir("/home/../.."), "/");
        // A name that merely begins with dots is a name, not a climb.
        assert_eq!(normalize_dir("/home/...oculto"), "/home/...oculto");
        assert_eq!(normalize_dir("/home/..b/c"), "/home/..b/c");
    }

    #[test]
    fn the_root_has_no_parent_so_backspace_stops_there() {
        // A parent-of-root answering "/" would leave a `..` row that goes
        // nowhere, and one answering "" would send `read_dir` to the
        // compositor's working directory.
        assert_eq!(parent_of("/"), None);
        assert_eq!(parent_of(""), None);
        assert_eq!(parent_of("/.."), None);
        // One level down from the root, the parent IS the root and not "".
        assert_eq!(parent_of("/home"), Some("/".into()));
        assert_eq!(parent_of("/home/"), Some("/".into()));
        assert_eq!(parent_of("/home/moebius"), Some("/home".into()));
        assert_eq!(
            parent_of("/home/moebius/docs/"),
            Some("/home/moebius".into())
        );
        // And climbing from anywhere reaches the root in finitely many steps
        // and then stops, rather than looping.
        let mut p = String::from("/a/b/c/d/e");
        let mut steps = 0;
        while let Some(up) = parent_of(&p) {
            p = up;
            steps += 1;
            assert!(steps < 10, "parent_of is not climbing");
        }
        assert_eq!(p, "/");
        assert_eq!(steps, 5);
    }

    #[test]
    fn a_child_is_joined_without_doubling_or_losing_a_slash() {
        assert_eq!(join_child("/home", "moebius"), "/home/moebius");
        assert_eq!(join_child("/home/", "moebius"), "/home/moebius");
        // From the root: "//moebius" would be a different path on some systems
        // and is not the entry that was clicked.
        assert_eq!(join_child("/", "moebius"), "/moebius");
        // A name with a space or an accent in it survives intact.
        assert_eq!(join_child("/home", "mis cosas"), "/home/mis cosas");
        assert_eq!(join_child("/home", "año"), "/home/año");
        // A dotfile is a child, not a climb.
        assert_eq!(join_child("/home", ".bashrc"), "/home/.bashrc");
    }

    // ── the listing ──────────────────────────────────────────────────────────

    #[test]
    fn directories_come_first_and_then_names_with_case_ignored() {
        let mut v = vec![
            file("zeta.txt", 1),
            dir("Vídeos"),
            file("Alfa.txt", 2),
            dir("apps"),
            file("beta.txt", 3),
        ];
        sort_entries(&mut v);
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        // Both directories ahead of every file, and `apps` before `Vídeos`
        // despite the capital: a case-sensitive sort would put every
        // capitalised name in a block of its own at the top.
        assert_eq!(
            names,
            ["apps", "Vídeos", "Alfa.txt", "beta.txt", "zeta.txt"]
        );
    }

    #[test]
    fn two_names_that_differ_only_in_case_keep_a_stable_order() {
        // `Makefile` and `makefile` are two files. A comparator that called
        // them equal would let them swap places between two listings of the
        // same unchanged directory, which reads as the list flickering.
        let mut v = vec![
            file("makefile", 1),
            file("Makefile", 2),
            file("MAKEFILE", 3),
        ];
        sort_entries(&mut v);
        let first: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        let mut v2 = vec![
            file("MAKEFILE", 3),
            file("makefile", 1),
            file("Makefile", 2),
        ];
        sort_entries(&mut v2);
        let second: Vec<&str> = v2.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(first, second, "the order depends on the input order");
        assert_eq!(first, ["MAKEFILE", "Makefile", "makefile"]);
    }

    #[test]
    fn hidden_files_are_shown_only_when_asked_for() {
        let v = vec![
            dir(".config"),
            dir("docs"),
            file(".bashrc", 1),
            file("notas.txt", 2),
        ];
        let shown: Vec<&str> = visible(&v, "", false)
            .iter()
            .map(|i| v[*i].name.as_str())
            .collect();
        assert_eq!(shown, ["docs", "notas.txt"]);
        let all: Vec<&str> = visible(&v, "", true)
            .iter()
            .map(|i| v[*i].name.as_str())
            .collect();
        assert_eq!(all, [".config", "docs", ".bashrc", "notas.txt"]);
        // `..` is not an entry and never comes from read_dir, so the dot rule
        // applies to everything it does see.
        assert!(is_hidden(".bashrc"));
        assert!(is_hidden(".."));
        assert!(!is_hidden("bashrc"));
        assert!(!is_hidden(""));
        // A name ending in a dot is not hidden.
        assert!(!is_hidden("informe."));
    }

    #[test]
    fn the_filter_ignores_case_and_accents_and_matches_anywhere() {
        let v = vec![
            file("Informe final.PDF", 1),
            file("notas.txt", 2),
            dir("Año 2026"),
        ];
        let hit = |f: &str| -> Vec<&str> {
            visible(&v, f, false)
                .iter()
                .map(|i| v[*i].name.as_str())
                .collect()
        };
        // An extension typed in lower case finds an upper-case one.
        assert_eq!(hit("pdf"), ["Informe final.PDF"]);
        // A substring from the middle counts.
        assert_eq!(hit("final"), ["Informe final.PDF"]);
        // Accents fold, both ways: `ano` finds `Año`.
        assert_eq!(hit("ano"), ["Año 2026"]);
        assert_eq!(hit("año"), ["Año 2026"]);
        // An empty or whitespace filter matches everything, not nothing.
        assert_eq!(hit("").len(), 3);
        assert_eq!(hit("   ").len(), 3);
        // And nothing matches nothing.
        assert!(hit("zzz").is_empty());
        // The indices are into the ORIGINAL slice: the browser resolves a row
        // back to an entry through them, so a filter that renumbered would open
        // the wrong file.
        let idx = visible(&v, "notas", false);
        assert_eq!(idx, vec![1]);
        assert_eq!(v[idx[0]].name, "notas.txt");
    }

    #[test]
    fn a_filter_keeps_hidden_files_hidden() {
        // The two filters are independent: typing `conf` must not pull
        // `.config` back into a listing where hidden files are off, or the
        // dotfiles reappear as soon as anyone types.
        let v = vec![dir(".config"), file("configuracion.txt", 1)];
        let shown: Vec<&str> = visible(&v, "conf", false)
            .iter()
            .map(|i| v[*i].name.as_str())
            .collect();
        assert_eq!(shown, ["configuracion.txt"]);
        assert_eq!(visible(&v, "conf", true).len(), 2);
    }

    // ── sizes ────────────────────────────────────────────────────────────────

    #[test]
    fn a_size_reads_in_the_unit_it_belongs_to_and_never_runs_off_the_table() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1), "1 B");
        assert_eq!(human_size(1023), "1023 B");
        // The unit changes exactly at the number it divides by.
        assert_eq!(human_size(1024), "1,0 K");
        assert_eq!(human_size(1536), "1,5 K");
        assert_eq!(human_size(1024 * 1024), "1,0 M");
        assert_eq!(human_size(1024 * 1024 * 1024), "1,0 G");
        // One decimal below ten of a unit, none above, so the column does not
        // jitter between `9,9 M` and `10,0 M`.
        assert_eq!(human_size(10 * 1024 * 1024), "10 M");
        assert_eq!(human_size(999 * 1024 * 1024), "999 M");
        // u64::MAX must land in the last unit rather than index past the table.
        let biggest = human_size(u64::MAX);
        assert!(biggest.ends_with(" P"), "{biggest}");
        // Every size in a wide sweep produces a unit from the table and nothing
        // with an `inf` or a `NaN` in it.
        for shift in 0..64 {
            let s = human_size(1u64 << shift);
            assert!(!s.contains("inf") && !s.contains("NaN"), "{s}");
            assert!(
                ["B", "K", "M", "G", "T", "P"].contains(&s.rsplit(' ').next().unwrap()),
                "{s}"
            );
        }
    }

    // ── eliding ──────────────────────────────────────────────────────────────

    #[test]
    fn a_long_path_is_cut_in_the_middle_and_keeps_both_ends() {
        // Both ends carry the information: where, and what. A head cut would
        // leave `/home/moebius/documentos/trab…` and tell the user neither.
        let p = "/home/moebius/documentos/trabajo/informes/informe-final.pdf";
        let cut = elide_middle(p, 30);
        assert_eq!(cut.chars().count(), 30);
        assert!(cut.starts_with("/home/"), "{cut}");
        assert!(cut.ends_with("final.pdf"), "{cut}");
        assert!(cut.contains('…'));
        // Something that already fits is returned untouched, ellipsis and all.
        assert_eq!(elide_middle("/home", 30), "/home");
        assert_eq!(elide_middle("/home", 5), "/home");
        // Counted in characters, not bytes: a path of accents cut to 10 comes
        // back as 10 characters and valid UTF-8, not sliced through a code
        // point (which would panic on a byte slice).
        let acc = "/home/moebius/añoañoañoaño/documentación.pdf";
        for max in 0..acc.chars().count() + 3 {
            let c = elide_middle(acc, max);
            assert!(c.chars().count() <= max, "max {max}: {c}");
        }
        assert_eq!(elide_middle("áéíóúáéíóú", 5).chars().count(), 5);
        // Degenerate widths do not panic and do not come back longer than asked.
        assert_eq!(elide_middle("abcdef", 0), "");
        assert_eq!(elide_middle("abcdef", 1), "a");
        assert_eq!(elide_middle("abcdef", 2), "ab");
        // Three characters is the narrowest middle cut there is: one from each
        // end around the ellipsis, not the first two and an ellipsis.
        assert_eq!(elide_middle("abcdef", 3), "a…f");
        assert_eq!(elide_middle("abcdef", 4), "ab…f");
        assert_eq!(elide_middle("abcdef", 5), "ab…ef");
    }

    // ── quoting ──────────────────────────────────────────────────────────────

    #[test]
    fn a_filename_cannot_become_a_command() {
        // THE one that matters. `spawn_detached` is `/bin/sh -c`, so a file
        // called `x'; reboot; '.txt` is a reboot the moment it is opened unless
        // the quoting holds. Single quotes have no escapes inside them, so the
        // only thing to handle is a single quote itself.
        assert_eq!(shell_quote("notas.txt"), "'notas.txt'");
        assert_eq!(shell_quote("mis cosas/a b"), "'mis cosas/a b'");
        assert_eq!(shell_quote("$HOME`id`"), "'$HOME`id`'");
        assert_eq!(shell_quote("a;reboot"), "'a;reboot'");
        assert_eq!(shell_quote("a&b|c>d<e"), "'a&b|c>d<e'");
        assert_eq!(shell_quote(""), "''");
        // The quote itself: close, escape, reopen.
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
        assert_eq!(shell_quote("'"), r#"''\'''"#);
        assert_eq!(
            shell_quote("x'; reboot; '.txt"),
            r#"'x'\''; reboot; '\''.txt'"#
        );
        // Whatever the name, the result is ONE shell word: outside the quotes
        // there is nothing but the escaped quotes themselves, so nothing in the
        // name can start a new command or a new argument.
        for name in [
            "a",
            "a b",
            "it's",
            "'",
            "''",
            "a'b'c",
            "x'; reboot; '.txt",
            "\n",
            "\t ",
            "*",
            "?",
            "~",
            "a\\'b",
            "$(id)",
            "año'ñ",
        ] {
            let q = shell_quote(name);
            assert!(q.starts_with('\'') && q.ends_with('\''), "{name:?} -> {q}");
            // Strip every `'\''` sequence and no bare quote may be left.
            let body = q[1..q.len() - 1].replace(r"'\''", "");
            assert!(!body.contains('\''), "{name:?} -> {q} leaks a quote");
        }
    }

    // ── where it opens ───────────────────────────────────────────────────────

    #[test]
    fn the_browser_opens_in_home_and_falls_back_to_the_root() {
        assert_eq!(default_dir(Some("/home/moebius")), "/home/moebius");
        assert_eq!(default_dir(Some("/home/moebius/")), "/home/moebius");
        // A relative or empty HOME must NOT be joined onto the compositor's
        // working directory, which is where a keybind-started client starts.
        assert_eq!(default_dir(Some("moebius")), "/");
        assert_eq!(default_dir(Some("")), "/");
        assert_eq!(default_dir(None), "/");
    }

    // ── the real filesystem ──────────────────────────────────────────────────

    #[test]
    fn a_listing_of_a_real_directory_is_sorted_and_keeps_what_it_cannot_stat() {
        // The one test that touches the disk: it proves `read_dir_sorted`
        // actually fills `is_dir` and `size` from metadata, which no amount of
        // testing `sort_entries` can.
        let base = std::env::temp_dir().join(format!("lunarfile-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("subdir")).unwrap();
        std::fs::write(base.join("zeta.txt"), b"1234567890").unwrap();
        std::fs::write(base.join("Alfa.txt"), b"").unwrap();
        std::fs::write(base.join(".oculto"), b"x").unwrap();
        // A dangling symlink: `metadata` fails on it, and a listing that
        // dropped what it could not stat would lose the file.
        #[cfg(unix)]
        std::os::unix::fs::symlink("/no/existe", base.join("roto")).unwrap();
        // A symlink to a directory: it must open like one, which is what
        // `DirEntry::metadata` (an lstat on Unix) got wrong.
        #[cfg(unix)]
        std::os::unix::fs::symlink(base.join("subdir"), base.join("atajo")).unwrap();

        let v = read_dir_sorted(base.to_str().unwrap()).unwrap();
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        // The directory leads, whatever its name's case, and the files follow
        // it in case-folded order -- dotfiles included, since `read_dir_sorted`
        // lists everything and `visible` is what hides them.
        assert_eq!(
            names,
            ["atajo", "subdir", ".oculto", "Alfa.txt", "roto", "zeta.txt"],
            "the directories lead, and a symlink to one is a directory"
        );
        let by = |n: &str| v.iter().find(|e| e.name == n).cloned();
        assert!(v[0].is_dir);
        assert_eq!(by("zeta.txt").unwrap().size, 10);
        assert!(!by("zeta.txt").unwrap().is_dir);
        assert_eq!(by("Alfa.txt").unwrap().size, 0);
        assert!(
            by(".oculto").is_some(),
            "a dotfile is listed, just not shown"
        );
        #[cfg(unix)]
        {
            let roto = by("roto").expect("a dangling symlink is still an entry");
            assert!(roto.link, "it must be marked a link");
            assert!(!roto.is_dir);
            assert_eq!(
                roto.size, 0,
                "nothing to stat means no size -- an lstat would say 10, the                  length of `/no/existe`"
            );
            let atajo = by("atajo").expect("a symlinked directory is an entry");
            assert!(atajo.is_dir, "a symlink to a directory must open like one");
            assert!(atajo.link);
        }
        // A directory that is not there is an error, not an empty listing: the
        // browser says so instead of showing a convincing empty folder.
        assert!(read_dir_sorted(base.join("no-existe").to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_left_out_rather_than_shown_wrong() {
        // A Unix filename is bytes. `to_string_lossy` would put U+FFFD where
        // the bad bytes are, and the row would then name a path that does not
        // exist: nothing could open it, and two different byte names collapse
        // into the SAME row, so activating one answers with the other's path.
        // Everything downstream is `String` -- `--out`, the portal's URIs --
        // so the honest cheap answer is to leave such a name out.
        use std::os::unix::ffi::OsStrExt;
        let base = std::env::temp_dir().join(format!("lunarfile-raro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("bueno.txt"), b"ok").unwrap();
        // Two different names that lossy conversion would turn into one.
        for bytes in [b"mal\xff.txt".as_slice(), b"mal\xfe.txt".as_slice()] {
            let raw = std::ffi::OsStr::from_bytes(bytes);
            std::fs::write(base.join(raw), b"x").unwrap();
        }
        // The filesystem under /tmp has to accept such a name for this to be a
        // test of anything; where it does not, there is nothing to skip.
        let on_disk = std::fs::read_dir(&base).unwrap().count();
        if on_disk == 3 {
            let v = read_dir_sorted(base.to_str().unwrap()).unwrap();
            let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["bueno.txt"], "a name that is not UTF-8 got through");
            // And nothing it kept carries the replacement character, which is
            // what a lossy listing would be full of.
            assert!(v.iter().all(|e| !e.name.contains('\u{fffd}')));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_absurdly_long_name_is_bounded_before_it_is_stored() {
        // Driven by whatever is on disk, so it needs a ceiling that is not the
        // filesystem's: a listing of thousands of entries each holding a name
        // nothing can draw is an allocation the user did not ask for.
        let long = "ñ".repeat(4000);
        let cut = crate::fill_guard::truncate_chars(&long, MAX_NAME_CHARS);
        assert_eq!(cut.chars().count(), MAX_NAME_CHARS);
        // A legal Linux name is 255 bytes, so the ceiling has to clear it or
        // ordinary files come back with their names cut.
        const { assert!(MAX_NAME_CHARS >= 255) };
    }
}
