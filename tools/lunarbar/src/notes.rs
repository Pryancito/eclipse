//! The notification history: every card the bar has shown, newest first, so a
//! notification that came and went while you were looking elsewhere can still
//! be read.
//!
//! The bar writes it (`hunter_toast`, as each card appears) and `lunarnotes`
//! reads it. A file and not a socket for the same reason as the desktop map
//! `lunardesk` reads: the bar has no event loop for anything but Wayland and a
//! one-second tick, and a reader that starts while the bar restarts still finds
//! what was there. It lives under `$XDG_RUNTIME_DIR`, so a reboot forgets it,
//! which is what a notification is for.
//!
//! Beside it, `seen` is a stamp: the time of the newest entry the user has
//! looked at. The bar's chip counts the entries newer than it, and opening
//! `lunarnotes` moves it.

use std::path::{Path, PathBuf};

/// Entries kept; past this the oldest go.
pub const MAX_ENTRIES: usize = 100;
/// No field is longer than this many characters.
const MAX_FIELD: usize = 512;

/// One notification as it was shown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// Seconds since the epoch, when it appeared.
    pub when: u64,
    /// `info`, `warn` or `crit`.
    pub severity: String,
    /// The program that sent it; empty for hunter's own alerts.
    pub app: String,
    pub title: String,
    pub body: String,
}

/// One field of a line: no tab (the separator) and no newline (the record).
fn field(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_FIELD)
        .collect::<String>()
        .trim()
        .to_string()
}

/// The history file and the stamp beside it.
pub fn paths(runtime_dir: Option<&str>) -> (PathBuf, PathBuf) {
    let base = match runtime_dir {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from("/run/user/0"),
    };
    let dir = base.join("lunar");
    (dir.join("notes"), dir.join("notes.seen"))
}

/// The paths for this session.
pub fn session_paths() -> (PathBuf, PathBuf) {
    paths(std::env::var("XDG_RUNTIME_DIR").ok().as_deref())
}

/// One line: `when<TAB>severity<TAB>app<TAB>title<TAB>body`.
pub fn encode(e: &Entry) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\n",
        e.when,
        field(&e.severity),
        field(&e.app),
        field(&e.title),
        field(&e.body)
    )
}

/// The entries of a history file, newest first. A line that does not parse is
/// skipped, not the whole file.
pub fn decode(text: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(5, '\t');
            let when = f.next()?.parse().ok()?;
            let severity = f.next()?.to_string();
            let app = f.next()?.to_string();
            let title = f.next()?.to_string();
            let body = f.next().unwrap_or("").to_string();
            (!title.is_empty()).then_some(Entry {
                when,
                severity,
                app,
                title,
                body,
            })
        })
        .collect();
    // The file is appended to, so it is oldest first; the reader wants the
    // newest on top. A stable sort keeps two entries of one second in order.
    out.reverse();
    out.sort_by_key(|e| std::cmp::Reverse(e.when));
    out.truncate(MAX_ENTRIES);
    out
}

pub fn load(path: &Path) -> Vec<Entry> {
    std::fs::read_to_string(path)
        .map(|t| decode(&t))
        .unwrap_or_default()
}

/// Add `e` to the history, keeping the newest [`MAX_ENTRIES`]. The file is
/// rewritten through a temporary beside it, so a reader never sees half.
pub fn record(path: &Path, e: &Entry) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut all = load(path);
    all.insert(0, e.clone());
    all.truncate(MAX_ENTRIES);
    let text: String = all.iter().rev().map(encode).collect();
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The newest entry the user has seen, as its time; 0 when never.
pub fn seen(stamp: &Path) -> u64 {
    std::fs::read_to_string(stamp)
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0)
}

/// Mark everything up to `when` as seen.
pub fn mark_seen(stamp: &Path, when: u64) -> std::io::Result<()> {
    if let Some(dir) = stamp.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(stamp, format!("{when}\n"))
}

/// How many entries are newer than the stamp.
pub fn unseen(entries: &[Entry], seen: u64) -> usize {
    entries.iter().filter(|e| e.when > seen).count()
}

/// Forget everything.
pub fn clear(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// "hace 3 min", "hace 2 h", or the time of day: how long ago, read at a
/// glance, for a list where the exact second does not matter.
pub fn ago(when: u64, now: u64) -> String {
    let d = now.saturating_sub(when);
    if d < 60 {
        "ahora".to_string()
    } else if d < 3600 {
        format!("hace {} min", d / 60)
    } else if d < 86_400 {
        format!("hace {} h", d / 3600)
    } else {
        format!("hace {} d", d / 86_400)
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(when: u64, title: &str) -> Entry {
        Entry {
            when,
            severity: "info".into(),
            app: "Firefox".into(),
            title: title.into(),
            body: "cuerpo".into(),
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lunarnotes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_line_round_trips_and_cannot_carry_a_second_record() {
        let x = Entry {
            when: 5,
            severity: "crit".into(),
            app: "a\tb".into(),
            title: "uno\ndos".into(),
            body: "c".into(),
        };
        let line = encode(&x);
        assert_eq!(line.matches('\n').count(), 1);
        assert_eq!(line.matches('\t').count(), 4);
        let back = decode(&line);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].app, "a b");
        assert_eq!(back[0].title, "uno dos");
    }

    #[test]
    fn the_history_is_newest_first_and_capped() {
        let dir = scratch("cap");
        let (path, _) = paths(Some(dir.to_str().unwrap()));
        for i in 0..MAX_ENTRIES as u64 + 5 {
            record(&path, &e(1000 + i, &format!("n{i}"))).unwrap();
        }
        let all = load(&path);
        assert_eq!(all.len(), MAX_ENTRIES);
        assert_eq!(all[0].title, format!("n{}", MAX_ENTRIES + 4));
        assert!(!all.iter().any(|x| x.title == "n0"));
        clear(&path).unwrap();
        assert!(load(&path).is_empty());
        clear(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_entries_of_the_same_second_keep_their_order() {
        let text = encode(&e(7, "primero")) + &encode(&e(7, "segundo"));
        let all = decode(&text);
        assert_eq!(all[0].title, "segundo");
        assert_eq!(all[1].title, "primero");
    }

    #[test]
    fn a_broken_line_is_skipped_and_not_the_file() {
        let text = format!("basura\n{}x\ty\n", encode(&e(3, "bien")));
        let all = decode(&text);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "bien");
    }

    #[test]
    fn the_unseen_count_is_what_came_after_the_stamp() {
        let dir = scratch("seen");
        let (path, stamp) = paths(Some(dir.to_str().unwrap()));
        let all = vec![e(30, "c"), e(20, "b"), e(10, "a")];
        assert_eq!(seen(&stamp), 0);
        assert_eq!(unseen(&all, seen(&stamp)), 3);
        mark_seen(&stamp, 20).unwrap();
        assert_eq!(unseen(&all, seen(&stamp)), 1);
        assert_eq!(path.parent(), stamp.parent());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ago_reads_like_a_person_would_say_it() {
        assert_eq!(ago(100, 130), "ahora");
        assert_eq!(ago(100, 100 + 5 * 60), "hace 5 min");
        assert_eq!(ago(0, 7200), "hace 2 h");
        assert_eq!(ago(0, 3 * 86_400), "hace 3 d");
        // A clock that went back is "now", not a panic.
        assert_eq!(ago(500, 100), "ahora");
    }

    #[test]
    fn the_paths_follow_the_runtime_directory() {
        assert_eq!(paths(Some("/r")).0, PathBuf::from("/r/lunar/notes"));
        assert_eq!(paths(Some("")).0, PathBuf::from("/run/user/0/lunar/notes"));
        assert_eq!(paths(None).1, PathBuf::from("/run/user/0/lunar/notes.seen"));
    }
}
