//! The clipboard history: what is kept, how it is stored and how the picker
//! asks the daemon for it.
//!
//! Everything in this module is pure or touches one file, with no Wayland and
//! no sockets, because it is the part that has to be right: a history that
//! loses the entry you just copied, or a decoder that throws the whole file
//! away because its last record was half-written when the machine went down,
//! is worse than no history at all. The protocol parts live here for the same
//! reason — `lunarclip --pick` and `lunarclip --watch` are two processes that
//! have to agree on a framing, and a framing is exactly the kind of thing that
//! is easy to write twice and impossible to keep in sync.
//!
//! Wayland has no clipboard in the X11 sense: the selection is a *client*
//! offering data, so when that client exits the selection goes with it, and
//! nothing in the session remembers what it said. That is what a clipboard
//! manager is for, and `zwlr_data_control_unstable_v1` is the protocol that
//! lets one watch and set the selection without holding the keyboard focus.

use std::path::{Path, PathBuf};

// ── what is kept ─────────────────────────────────────────────────────────────

/// How many entries the history holds. Past this the oldest falls off.
///
/// A hundred is what the picker can plausibly be searched through and what
/// keeps the file small enough to rewrite on every copy without thinking about
/// it (a hundred entries of the per-entry cap is a worst case nobody reaches;
/// the real file is a few kilobytes).
pub const MAX_ENTRIES: usize = 100;

/// The biggest single entry that is remembered, in bytes.
///
/// A selection larger than this is still *served* by the compositor while the
/// source application lives — this cap is only about what gets written to the
/// history file and shown in the picker. Copying a 40 MiB build log should not
/// grow the session's memory by 40 MiB forever, and no human is going to pick
/// it out of a list by its first line.
pub const MAX_ENTRY_BYTES: usize = 256 * 1024;

/// MIME types a text selection can be offered under, best first.
///
/// `text/plain;charset=utf-8` is what every Wayland client offers and what
/// `wl-clipboard` asks for by default. The X11 atom names below it arrive
/// through Xwayland, where a copy from `xterm` or an old Motif application
/// offers `UTF8_STRING`/`STRING` and nothing else; without them a copy from an
/// X11 client would land in the history as "no text here".
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

/// MIME types that mean "this is a password, do not keep it".
///
/// Password managers advertise one of these alongside the text so a clipboard
/// manager can tell a secret apart from a selection. KDE's hint is the one
/// Plasma and KeePassXC set, and it is honoured by every manager worth using.
/// Eclipse honours it too: a history file on disk that quietly accumulates
/// every password the user ever copied is a security hole with a UI.
const SECRET_MIMES: &[&str] = &[
    "x-kde-passwordManagerHint",
    "application/x-libreoffice-password",
];

/// Whether `mimes` says this selection must not be remembered.
pub fn is_secret(mimes: &[String]) -> bool {
    mimes
        .iter()
        .any(|m| SECRET_MIMES.iter().any(|s| m.eq_ignore_ascii_case(s)))
}

/// The best text MIME among the ones an offer advertised, or None when the
/// selection carries no text at all (an image, a file list, a drag of a URL
/// list from a browser that offers nothing else).
///
/// `None` is deliberately not an error: a selection with no text is a
/// selection this history does not handle, and the right answer is to leave
/// the previous entries alone rather than to clear them.
pub fn best_text_mime(mimes: &[String]) -> Option<String> {
    for want in TEXT_MIMES {
        if let Some(m) = mimes.iter().find(|m| m.eq_ignore_ascii_case(want)) {
            return Some(m.clone());
        }
    }
    // Anything else that is text: a client offering only `text/html` or
    // `text/x-moz-url` still copied something a person can paste into a
    // terminal, and taking it beats dropping the copy on the floor.
    mimes
        .iter()
        .find(|m| {
            let m = m.to_ascii_lowercase();
            m.starts_with("text/") && !m.starts_with("text/uri-list")
        })
        .cloned()
}

/// Whether `text` is worth a history entry.
///
/// Whitespace-only selections are not: a double-click that lands between two
/// words, or a mouse-up after a drag that selected nothing, both arrive here as
/// a selection event with a space or an empty string in it, and a history whose
/// first entry is "   " is a history the user has to scroll past every time.
pub fn worth_keeping(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_ENTRY_BYTES
}

/// The clipboard history, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    items: Vec<String>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// The entries, newest first.
    pub fn items(&self) -> &[String] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// What is on the clipboard now, as far as this history knows.
    pub fn newest(&self) -> Option<&str> {
        self.items.first().map(String::as_str)
    }

    pub fn get(&self, index: usize) -> Option<&str> {
        self.items.get(index).map(String::as_str)
    }

    /// Remember `text`, and say whether the history actually changed.
    ///
    /// A re-copy of something already in the list **moves it to the front**
    /// rather than adding a second copy: the list is a most-recently-used
    /// order, and a manager that lets the same URL in forty times has a
    /// history forty entries long with one thing in it.
    ///
    /// The `false` return is what keeps the watcher from re-announcing a
    /// selection it set itself: re-copying the newest entry is a no-op, so the
    /// daemon has nothing to write and nothing to re-offer, and the loop that
    /// every clipboard manager is one bug away from does not start.
    pub fn push(&mut self, text: &str) -> bool {
        if !worth_keeping(text) {
            return false;
        }
        if self.items.first().map(String::as_str) == Some(text) {
            return false;
        }
        self.items.retain(|i| i != text);
        self.items.insert(0, text.to_string());
        self.items.truncate(MAX_ENTRIES);
        true
    }

    /// Move the entry at `index` to the front, as picking it from the list
    /// does, and return it. Out of range leaves the history untouched.
    pub fn promote(&mut self, index: usize) -> Option<String> {
        if index >= self.items.len() {
            return None;
        }
        let text = self.items.remove(index);
        self.items.insert(0, text.clone());
        Some(text)
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// The entries whose text contains `needle`, case-insensitively, with the
    /// index each one has in the full history.
    ///
    /// The index travels with the row because the picker sets the selection by
    /// index: filtering renumbers the rows on the screen, and a `SET 3` that
    /// means "the fourth row of the filtered list" would paste whatever
    /// happens to be fourth in the unfiltered one.
    pub fn search(&self, needle: &str) -> Vec<(usize, &str)> {
        let needle = needle.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, t)| needle.is_empty() || t.to_lowercase().contains(&needle))
            .map(|(i, t)| (i, t.as_str()))
            .collect()
    }
}

// ── one line of an entry ─────────────────────────────────────────────────────

/// A clipboard entry as one line of the picker.
///
/// Three things happen here. Newlines become `¶`, because the picker draws one
/// row per entry and a multi-line entry drawn raw would run off the row and
/// over the next one. Runs of whitespace collapse, so an indented block of
/// code does not show as a row that starts with twenty blanks and says nothing.
/// And everything outside ISO-8859-1 becomes `?`: the font in `draw.rs` is a
/// bitmap ISO-8859-1 face, so a UTF-8 entry drawn as-is would paint the
/// *bytes* of its characters — the one-character replacement keeps the row's
/// length honest, which is what the truncation below depends on.
pub fn preview(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for ch in text.chars() {
        let ch = match ch {
            '\n' | '\r' => '¶',
            '\t' => ' ',
            c if c.is_whitespace() => ' ',
            // Control characters a terminal copy can carry (an ESC from a
            // coloured `ls` output, a BEL) have no glyph and would draw as
            // whatever the font has at that slot.
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => '?',
            c if (c as u32) > 0xff => '?',
            c => c,
        };
        if ch == ' ' {
            // Leading whitespace is dropped entirely, inner runs collapse to
            // one blank, and a trailing run never gets written: `pending_space`
            // only becomes a character when a non-blank follows it.
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
        // One past the cap, so the caller can tell "exactly full" from
        // "longer than this" without walking the whole entry again.
        if out.chars().count() > max_chars {
            break;
        }
    }
    if out.chars().count() > max_chars {
        let keep = max_chars.saturating_sub(3);
        let mut cut: String = out.chars().take(keep).collect();
        cut.push_str("...");
        return cut;
    }
    out
}

/// The short "123 B" / "4,5 KiB" a row shows on its right, so an entry whose
/// preview is a line of a long file still says it is a long file.
pub fn size_label(bytes: usize) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kib = bytes as f64 / 1024.0;
    if kib < 10.0 {
        // One decimal below 10 KiB: between 1 and 10 KiB a whole number is a
        // tenfold range shown as one of nine values.
        return format!("{:.1} KiB", kib).replace('.', ",");
    }
    if kib < 1024.0 {
        return format!("{} KiB", kib.round() as u64);
    }
    format!("{:.1} MiB", kib / 1024.0).replace('.', ",")
}

// ── the file on disk ─────────────────────────────────────────────────────────

/// Where the history lives when `$XDG_CACHE_HOME` and `$HOME` say nothing.
const FALLBACK_DIR: &str = "/root/.cache/eclipse";

/// The history file for this user.
///
/// A cache directory and not `$XDG_DATA_HOME`: this file is a convenience that
/// can be deleted at any time without the session noticing, which is the whole
/// definition of a cache. It is also the directory `lunarbar` already writes
/// its icon cache into, so the session creates it anyway.
pub fn history_path(cache_home: Option<&str>, home: Option<&str>) -> PathBuf {
    let dir = match (cache_home, home) {
        (Some(c), _) if !c.is_empty() => PathBuf::from(c).join("eclipse"),
        (_, Some(h)) if !h.is_empty() => PathBuf::from(h).join(".cache").join("eclipse"),
        _ => PathBuf::from(FALLBACK_DIR),
    };
    dir.join("clipboard")
}

/// The history as the bytes of the file.
///
/// Records are `@<len>\n<len bytes>\n`, newest first. Length-prefixed and not
/// line-delimited because clipboard entries contain newlines — a line-based
/// format would need escaping, and an escaping bug turns a copied `\n` into a
/// broken entry on the next boot. The trailing newline after the payload is
/// not part of it; it is there so a person running `cat` on the file can read
/// it, and the decoder does not require it.
pub fn encode(items: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        out.extend_from_slice(format!("@{}\n", item.len()).as_bytes());
        out.extend_from_slice(item.as_bytes());
        out.push(b'\n');
    }
    out
}

/// The entries of an encoded history, stopping at the first thing that does not
/// decode.
///
/// **Stopping and not failing** is the point: the file is rewritten on every
/// copy, so a power cut lands in the middle of a write often enough to matter,
/// and the half-written record at the end is the *newest* one. Keeping the
/// records that did decode costs nothing and keeps a history the user spent a
/// session filling.
pub fn decode(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        // A stray newline between records (or a trailing one at the end of the
        // file) is not a malformed record.
        if rest[0] == b'\n' {
            rest = &rest[1..];
            continue;
        }
        let Some(head) = rest.strip_prefix(b"@") else {
            break;
        };
        let Some(nl) = head.iter().position(|b| *b == b'\n') else {
            break;
        };
        let Ok(digits) = std::str::from_utf8(&head[..nl]) else {
            break;
        };
        let Ok(len) = digits.parse::<usize>() else {
            break;
        };
        let body = &head[nl + 1..];
        if len > body.len() {
            break;
        }
        let Ok(text) = std::str::from_utf8(&body[..len]) else {
            break;
        };
        out.push(text.to_string());
        rest = &body[len..];
        if out.len() >= MAX_ENTRIES {
            break;
        }
    }
    out
}

/// Read the history file, or an empty history when it is not there.
pub fn load(path: &Path) -> History {
    let mut h = History::new();
    if let Ok(bytes) = std::fs::read(path) {
        // In file order, which is newest first, so they go in oldest first.
        for item in decode(&bytes).into_iter().rev() {
            h.push(&item);
        }
    }
    h
}

/// Write the history file, creating its directory, and replace it atomically.
///
/// The temporary file sits beside the real one so the rename cannot cross a
/// filesystem, and it carries the pid so two daemons (one being started while
/// the old one still runs, which is exactly what a respawn looks like) cannot
/// write each other's temporary.
pub fn save(path: &Path, h: &History) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, encode(h.items()))?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

// ── talking to the daemon ────────────────────────────────────────────────────

/// The daemon's socket, under the runtime directory the session already has.
///
/// `/run/user/0` is what the labwc session exports as `XDG_RUNTIME_DIR` and
/// where the Wayland socket itself lives, so a daemon that can reach the
/// compositor can reach this.
pub fn socket_path(runtime_dir: Option<&str>) -> PathBuf {
    match runtime_dir {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("lunarclip.sock"),
        _ => PathBuf::from("/run/user/0/lunarclip.sock"),
    }
}

/// What a client asks the daemon for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Req {
    /// Send the history, newest first.
    List,
    /// Put entry `index` of the history on the clipboard.
    Set(usize),
    /// Put this text on the clipboard (and at the front of the history).
    Copy(String),
    /// Forget everything.
    Clear,
}

/// How far `parse_req` got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReqParse {
    /// A complete request and how many bytes of the buffer it used.
    Done(Req, usize),
    /// Nothing wrong so far, but the rest has not arrived yet.
    Incomplete,
    /// Not a request this daemon speaks; the connection is dropped.
    Bad,
}

/// Parse one request off the front of `buf`.
///
/// The verbs are a line; `COPY` is a line followed by one length-prefixed
/// record, the same framing the file uses, for the same reason: the text can
/// contain anything, newlines included.
pub fn parse_req(buf: &[u8]) -> ReqParse {
    // The newline a `COPY` writes after its payload is not part of the record,
    // so it is still in the buffer when the next request is parsed. Eating
    // blank lines here is what makes two requests in one write work: without
    // it the second parse saw an empty verb and dropped the connection.
    let blanks = buf.iter().take_while(|b| **b == b'\n').count();
    let buf = &buf[blanks..];
    let Some(nl) = buf.iter().position(|b| *b == b'\n') else {
        // A client that never sends a newline is handled by the read loop's
        // own cap, not here: this function cannot see how much it has been
        // called with over time.
        return ReqParse::Incomplete;
    };
    let Ok(line) = std::str::from_utf8(&buf[..nl]) else {
        return ReqParse::Bad;
    };
    let line = line.trim_end_matches('\r');
    // Every `used` below counts from the ORIGINAL buffer, blank lines
    // included, because that is what the caller advances by.
    let used = blanks + nl + 1;
    let (verb, arg) = match line.split_once(' ') {
        Some((v, a)) => (v, a.trim()),
        None => (line, ""),
    };
    match verb {
        "LIST" => ReqParse::Done(Req::List, used),
        "CLEAR" => ReqParse::Done(Req::Clear, used),
        "SET" => match arg.parse::<usize>() {
            Ok(i) => ReqParse::Done(Req::Set(i), used),
            Err(_) => ReqParse::Bad,
        },
        "COPY" => {
            let rest = &buf[used..];
            let Some(head) = rest.strip_prefix(b"@") else {
                // The `@` has not arrived yet; `Bad` would drop a client whose
                // write was split across two packets.
                return if rest.is_empty() {
                    ReqParse::Incomplete
                } else {
                    ReqParse::Bad
                };
            };
            let Some(hnl) = head.iter().position(|b| *b == b'\n') else {
                return ReqParse::Incomplete;
            };
            let Ok(digits) = std::str::from_utf8(&head[..hnl]) else {
                return ReqParse::Bad;
            };
            let Ok(len) = digits.parse::<usize>() else {
                return ReqParse::Bad;
            };
            if len > MAX_ENTRY_BYTES {
                return ReqParse::Bad;
            }
            let body = &head[hnl + 1..];
            if body.len() < len {
                return ReqParse::Incomplete;
            }
            let Ok(text) = std::str::from_utf8(&body[..len]) else {
                return ReqParse::Bad;
            };
            // `used` + `@len\n` + the payload. A trailing newline after the
            // payload is optional and is eaten by the next parse.
            ReqParse::Done(Req::Copy(text.to_string()), used + 1 + hnl + 1 + len)
        }
        _ => ReqParse::Bad,
    }
}

/// The bytes of a request, as the client writes them.
pub fn encode_req(req: &Req) -> Vec<u8> {
    match req {
        Req::List => b"LIST\n".to_vec(),
        Req::Clear => b"CLEAR\n".to_vec(),
        Req::Set(i) => format!("SET {i}\n").into_bytes(),
        Req::Copy(text) => {
            let mut out = format!("COPY\n@{}\n", text.len()).into_bytes();
            out.extend_from_slice(text.as_bytes());
            out.push(b'\n');
            out
        }
    }
}

/// The daemon's answer to `LIST`: the records, then a `.` line.
///
/// The terminator is what lets a client read until the answer is complete
/// instead of until the daemon closes the socket, which it does not do — the
/// same connection can carry a `LIST` and then the `SET` the user picked.
pub fn encode_list(items: &[String]) -> Vec<u8> {
    let mut out = encode(items);
    out.extend_from_slice(b".\n");
    out
}

/// The entries of a `LIST` answer, or None while the `.` line has not arrived.
pub fn decode_list(bytes: &[u8]) -> Option<Vec<String>> {
    // The terminator is a `.` at the start of a line, which cannot be confused
    // with a record (those start with `@`) or with payload bytes (those are
    // inside a length the decoder already knows).
    let mut rest = bytes;
    let mut items = Vec::new();
    loop {
        if let Some(r) = rest.strip_prefix(b"\n") {
            rest = r;
            continue;
        }
        if rest.starts_with(b".") {
            return Some(items);
        }
        let head = rest.strip_prefix(b"@")?;
        let nl = head.iter().position(|b| *b == b'\n')?;
        let len = std::str::from_utf8(&head[..nl])
            .ok()?
            .parse::<usize>()
            .ok()?;
        let body = &head[nl + 1..];
        if body.len() < len {
            return None;
        }
        items.push(std::str::from_utf8(&body[..len]).ok()?.to_string());
        rest = &body[len..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_utf8_text_mime_wins_over_the_bare_one() {
        let m = mimes(&["text/html", "text/plain", "text/plain;charset=utf-8"]);
        assert_eq!(
            best_text_mime(&m).as_deref(),
            Some("text/plain;charset=utf-8")
        );
    }

    #[test]
    fn an_x11_copy_offers_only_atoms_and_is_still_text() {
        // What a selection from xterm through Xwayland looks like. Before the
        // atom names were in the table this came out as "no text".
        let m = mimes(&["TIMESTAMP", "TARGETS", "UTF8_STRING", "STRING", "TEXT"]);
        assert_eq!(best_text_mime(&m).as_deref(), Some("UTF8_STRING"));
    }

    #[test]
    fn mime_matching_ignores_case_as_the_atom_spellings_differ() {
        let m = mimes(&["TEXT/PLAIN;CHARSET=UTF-8"]);
        assert_eq!(
            best_text_mime(&m).as_deref(),
            Some("TEXT/PLAIN;CHARSET=UTF-8")
        );
    }

    #[test]
    fn an_image_only_selection_has_no_text_mime() {
        let m = mimes(&["image/png", "image/jpeg", "text/uri-list"]);
        assert_eq!(best_text_mime(&m), None);
    }

    #[test]
    fn an_unusual_text_mime_is_taken_rather_than_dropped() {
        let m = mimes(&["text/x-moz-url", "application/octet-stream"]);
        assert_eq!(best_text_mime(&m).as_deref(), Some("text/x-moz-url"));
    }

    #[test]
    fn a_password_manager_hint_keeps_the_entry_out_of_the_history() {
        let m = mimes(&["text/plain;charset=utf-8", "x-kde-passwordManagerHint"]);
        assert!(is_secret(&m));
        assert!(!is_secret(&mimes(&["text/plain;charset=utf-8"])));
    }

    #[test]
    fn whitespace_only_selections_are_not_worth_an_entry() {
        assert!(!worth_keeping(""));
        assert!(!worth_keeping("   \n\t "));
        assert!(worth_keeping(" x "));
    }

    #[test]
    fn an_entry_past_the_cap_is_not_remembered() {
        let big = "x".repeat(MAX_ENTRY_BYTES + 1);
        assert!(!worth_keeping(&big));
        assert!(worth_keeping(&"x".repeat(MAX_ENTRY_BYTES)));
    }

    #[test]
    fn a_recopy_moves_the_entry_to_the_front_without_duplicating_it() {
        let mut h = History::new();
        assert!(h.push("uno"));
        assert!(h.push("dos"));
        assert!(h.push("tres"));
        assert!(h.push("uno"));
        assert_eq!(h.items(), ["uno", "tres", "dos"]);
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn recopying_the_newest_entry_changes_nothing() {
        // This is what stops the watcher's feedback loop: the daemon re-offers
        // the selection it holds, the compositor hands it back as a new
        // selection event, and this `false` is where that stops.
        let mut h = History::new();
        assert!(h.push("uno"));
        assert!(!h.push("uno"));
        assert_eq!(h.items(), ["uno"]);
    }

    #[test]
    fn the_history_stops_at_its_cap_dropping_the_oldest() {
        let mut h = History::new();
        for i in 0..MAX_ENTRIES + 10 {
            assert!(h.push(&format!("e{i}")));
        }
        assert_eq!(h.len(), MAX_ENTRIES);
        assert_eq!(h.newest(), Some(format!("e{}", MAX_ENTRIES + 9).as_str()));
        // The first ten fell off the end, not off the front.
        assert!(!h.items().iter().any(|i| i == "e0"));
    }

    #[test]
    fn promote_moves_a_row_to_the_front_and_out_of_range_does_nothing() {
        let mut h = History::new();
        for t in ["c", "b", "a"] {
            h.push(t);
        }
        assert_eq!(h.items(), ["a", "b", "c"]);
        assert_eq!(h.promote(2).as_deref(), Some("c"));
        assert_eq!(h.items(), ["c", "a", "b"]);
        assert_eq!(h.promote(9), None);
        // Exactly one past the end as well: `remove(len)` panics, and a
        // picker whose list shrank under it sends exactly that index.
        assert_eq!(h.promote(h.len()), None);
        assert_eq!(h.items(), ["c", "a", "b"]);
    }

    #[test]
    fn search_carries_the_unfiltered_index_of_every_row() {
        let mut h = History::new();
        for t in ["tercero", "https://example.com", "primero"] {
            h.push(t);
        }
        let hits = h.search("ERO");
        assert_eq!(hits, vec![(0, "primero"), (2, "tercero")]);
        // An empty needle is every row, each with its own index.
        assert_eq!(h.search("").len(), 3);
        assert!(h.search("zzz").is_empty());
    }

    #[test]
    fn a_multiline_entry_previews_as_one_line() {
        assert_eq!(preview("uno\ndos", 40), "uno¶dos");
        assert_eq!(preview("\n", 40), "¶");
    }

    #[test]
    fn preview_collapses_indentation_and_drops_the_leading_blanks() {
        assert_eq!(
            preview("    fn main() {\n        x\n", 40),
            "fn main() {¶ x¶"
        );
        assert_eq!(preview("a   \t  b  ", 40), "a b");
    }

    #[test]
    fn preview_truncates_with_dots_and_never_exceeds_the_cap() {
        let p = preview("abcdefghij", 6);
        assert_eq!(p, "abc...");
        assert_eq!(p.chars().count(), 6);
        // Exactly the cap is not truncated: the "..." would lose three
        // characters to say nothing.
        assert_eq!(preview("abcdef", 6), "abcdef");
    }

    #[test]
    fn preview_counts_characters_and_not_bytes() {
        // Ten accented characters are twenty bytes; truncating on bytes would
        // cut this to five characters and could split one in half.
        let p = preview("áéíóúáéíóú", 10);
        assert_eq!(p.chars().count(), 10);
        assert_eq!(p, "áéíóúáéíóú");
    }

    #[test]
    fn preview_replaces_what_the_bitmap_font_cannot_draw() {
        // Beyond ISO-8859-1 (a CJK character, an emoji) and a control
        // character from a coloured terminal copy.
        assert_eq!(preview("a\u{4e2d}b", 40), "a?b");
        assert_eq!(preview("a\u{1f600}b", 40), "a?b");
        assert_eq!(preview("a\u{1b}[0mb", 40), "a?[0mb");
        // Inside ISO-8859-1 it is left alone.
        assert_eq!(preview("ñ€", 40), "ñ?");
    }

    #[test]
    fn size_labels_read_like_sizes() {
        assert_eq!(size_label(0), "0 B");
        assert_eq!(size_label(1023), "1023 B");
        assert_eq!(size_label(1024), "1,0 KiB");
        assert_eq!(size_label(5 * 1024 + 512), "5,5 KiB");
        assert_eq!(size_label(100 * 1024), "100 KiB");
        assert_eq!(size_label(3 * 1024 * 1024 / 2), "1,5 MiB");
    }

    #[test]
    fn the_history_file_round_trips_entries_with_newlines_in_them() {
        let items = vec![
            "una linea".to_string(),
            "dos\nlineas".to_string(),
            "@10\nfake record".to_string(),
            "".to_string(),
        ];
        assert_eq!(decode(&encode(&items)), items);
    }

    #[test]
    fn a_half_written_last_record_keeps_the_rest() {
        // What a power cut during `save` leaves behind. The truncated record is
        // the newest one; the entries from before the crash are still there.
        let mut bytes = encode(&["uno".to_string(), "dos".to_string()]);
        bytes.extend_from_slice(b"@99\nhalf");
        assert_eq!(decode(&bytes), vec!["uno".to_string(), "dos".to_string()]);
    }

    #[test]
    fn garbage_instead_of_a_record_stops_the_decoder_where_it_is() {
        let mut bytes = encode(&["uno".to_string()]);
        bytes.extend_from_slice(b"esto no es un registro\n@3\ntwo");
        assert_eq!(decode(&bytes), vec!["uno".to_string()]);
    }

    #[test]
    fn decode_refuses_to_grow_past_the_entry_cap() {
        let many: Vec<String> = (0..MAX_ENTRIES + 50).map(|i| format!("e{i}")).collect();
        assert_eq!(decode(&encode(&many)).len(), MAX_ENTRIES);
    }

    #[test]
    fn load_keeps_the_file_order_as_the_newest_first() {
        let dir = std::env::temp_dir().join(format!("lunarclip-load-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("clipboard");
        std::fs::write(&path, encode(&["nuevo".to_string(), "viejo".to_string()])).unwrap();
        let h = load(&path);
        assert_eq!(h.items(), ["nuevo", "viejo"]);
        // A file that is not there is an empty history, not a failure.
        assert!(load(&dir.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_the_directory_and_load_reads_it_back() {
        let dir = std::env::temp_dir().join(format!("lunarclip-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("clipboard");
        let mut h = History::new();
        h.push("uno\ndos");
        h.push("tres");
        save(&path, &h).unwrap();
        assert_eq!(load(&path), h);
        // And no temporary is left beside it.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "clipboard")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_history_path_follows_xdg_then_home_then_the_fallback() {
        assert_eq!(
            history_path(Some("/c"), Some("/h")),
            PathBuf::from("/c/eclipse/clipboard")
        );
        assert_eq!(
            history_path(None, Some("/h")),
            PathBuf::from("/h/.cache/eclipse/clipboard")
        );
        // An exported-but-empty variable is not a directory called "".
        assert_eq!(
            history_path(Some(""), Some("")),
            PathBuf::from("/root/.cache/eclipse/clipboard")
        );
        assert_eq!(
            history_path(None, None),
            PathBuf::from("/root/.cache/eclipse/clipboard")
        );
    }

    #[test]
    fn the_socket_sits_in_the_runtime_directory() {
        assert_eq!(
            socket_path(Some("/run/user/0")),
            PathBuf::from("/run/user/0/lunarclip.sock")
        );
        assert_eq!(
            socket_path(None),
            PathBuf::from("/run/user/0/lunarclip.sock")
        );
        assert_eq!(socket_path(Some("")), socket_path(None));
    }

    #[test]
    fn every_request_round_trips_through_its_own_encoding() {
        for req in [
            Req::List,
            Req::Clear,
            Req::Set(7),
            Req::Copy("con\nsalto".to_string()),
            Req::Copy(String::new()),
        ] {
            let bytes = encode_req(&req);
            match parse_req(&bytes) {
                ReqParse::Done(got, used) => {
                    assert_eq!(got, req);
                    // Everything but the optional trailing newline of a COPY.
                    assert!(used <= bytes.len() && bytes.len() - used <= 1, "{req:?}");
                }
                other => panic!("{req:?} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn two_requests_in_one_write_are_parsed_one_after_the_other() {
        // What the picker sends: LIST, then the SET of the row the user chose,
        // on the same connection, and possibly in the same packet.
        let mut buf = encode_req(&Req::Copy("hola".to_string()));
        buf.extend_from_slice(&encode_req(&Req::Set(2)));
        let ReqParse::Done(first, used) = parse_req(&buf) else {
            panic!("first request did not parse");
        };
        assert_eq!(first, Req::Copy("hola".to_string()));
        // Seven and not six: the `COPY` payload's trailing newline is still
        // in front of the `SET`, and the bytes it eats count as used.
        assert_eq!(parse_req(&buf[used..]), ReqParse::Done(Req::Set(2), 7));
    }

    #[test]
    fn a_request_split_across_packets_is_incomplete_and_not_bad() {
        let full = encode_req(&Req::Copy("un texto".to_string()));
        for cut in 1..full.len() - 1 {
            assert_eq!(
                parse_req(&full[..cut]),
                ReqParse::Incomplete,
                "cut at {cut} should wait for more"
            );
        }
    }

    #[test]
    fn nonsense_is_refused_rather_than_waited_on() {
        assert_eq!(parse_req(b"QUIT\n"), ReqParse::Bad);
        assert_eq!(parse_req(b"SET x\n"), ReqParse::Bad);
        assert_eq!(parse_req(b"SET\n"), ReqParse::Bad);
        assert_eq!(parse_req(b"COPY\nnot-a-record"), ReqParse::Bad);
        assert_eq!(parse_req(b"COPY\n@x\n"), ReqParse::Bad);
        // And a COPY bigger than any entry may be is refused before it is
        // buffered, which is what keeps a client from making the daemon hold
        // an arbitrary amount of memory.
        let huge = format!("COPY\n@{}\n", MAX_ENTRY_BYTES + 1);
        assert_eq!(parse_req(huge.as_bytes()), ReqParse::Bad);
    }

    #[test]
    fn a_list_answer_round_trips_and_waits_for_its_terminator() {
        let items = vec!["uno".to_string(), "dos\ntres".to_string()];
        let bytes = encode_list(&items);
        assert_eq!(decode_list(&bytes).as_ref(), Some(&items));
        assert_eq!(decode_list(&bytes[..bytes.len() - 2]), None);
        // An empty history is a valid answer, not a missing one.
        assert_eq!(decode_list(&encode_list(&[])), Some(Vec::new()));
    }
}
