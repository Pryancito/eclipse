//! What lunarfile can do to a file: copy and move it into a folder, rename
//! it, make a folder, and send it to the trash.
//!
//! None of these ever replaces anything. A name that is taken in the
//! destination becomes `name (copia)`, `name (copia 2)`, ... the way every
//! desktop's file manager answers it, and a rename onto a taken name is
//! refused. Deleting is the trash, never `unlink`: the freedesktop.org Trash
//! specification's `$XDG_DATA_HOME/Trash`, with its `.trashinfo`, so what is
//! sent there can be found and put back by any other file manager too.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::i18n::{Key, Lang};

// ── what this module refuses, said without a language ────────────────────────
//
// The four errors fileops raises ITSELF carry a tag rather than a sentence,
// and [`message`] turns a tag into the session's language. It is the same
// split `netconf::check` uses: the operation decides WHAT is wrong, the window
// decides how to say it. The alternative -- a Spanish sentence inside
// `io::Error` -- put Spanish on an English panel under an English label, which
// is worse than either language on its own.
//
// The tags are not shown to anybody, so they are ASCII and stable; they are
// matched on, so they must not collide with a message the C library produces.

const T_CANCELLED: &str = "eclipse-fileops:cancelled";
const T_BAD_NAME: &str = "eclipse-fileops:bad-name";
const T_SELF_NEST: &str = "eclipse-fileops:self-nest";
const T_EXISTS: &str = "eclipse-fileops:exists";

/// `e` as the panel should say it: the row of the string table for an error
/// this module raised, and the system's own text for anything else.
///
/// An `io::Error` from the kernel (`Permission denied`, `No space left on
/// device`) is NOT translated: it is the C library's string, it is what the
/// same failure says in every log on the machine, and inventing a Spanish
/// wording for each `errno` is a translation of the C library, not of Eclipse.
pub fn message(e: &io::Error, lang: Lang) -> String {
    let text = e.to_string();
    let key = match text.as_str() {
        T_CANCELLED => Key::file_err_cancelled,
        T_BAD_NAME => Key::file_err_bad_name,
        T_SELF_NEST => Key::file_err_self_nest,
        T_EXISTS => Key::file_err_exists,
        _ => return text,
    };
    lang.s(key).to_string()
}

/// How far a copy has got, shared between the thread doing it and the
/// window drawing it: bytes `done` of `total`, and a flag the window raises
/// to stop it.
#[derive(Debug, Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    /// The fraction done, 0 to 1; an empty copy is done.
    pub fn frac(&self) -> f32 {
        let (d, t) = (self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed));
        if t == 0 {
            1.0
        } else {
            (d as f64 / t as f64).min(1.0) as f32
        }
    }
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, T_CANCELLED)
}

/// The bytes under `p`: a file's size, a folder's files' sizes added up. A
/// link counts as nothing, since it is copied as a link.
pub fn tree_size(p: &Path) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(p) else { return 0 };
    if md.is_dir() {
        std::fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| tree_size(&e.path())).sum())
            .unwrap_or(0)
    } else if md.file_type().is_symlink() {
        0
    } else {
        md.len()
    }
}

/// Copy one file in 1 MiB steps, counting into `prog` and stopping when it
/// is cancelled.
fn copy_file(src: &Path, dst: &Path, prog: &Progress) -> io::Result<()> {
    use std::io::{Read, Write};
    let mut r = std::fs::File::open(src)?;
    let mut w = std::fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if prog.cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
        prog.done.fetch_add(n as u64, Ordering::Relaxed);
    }
    std::fs::set_permissions(dst, std::fs::metadata(src)?.permissions())
}

/// A name a file can have: not empty, not `.` or `..`, no `/`, no NUL.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

fn bad_name() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, T_BAD_NAME)
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// `name`, or the first of `stem (copia).ext`, `stem (copia 2).ext`, ... not
/// taken in `dir`. The extension stays last so the copy still opens with
/// what opened the original; a folder or a dotfile has none.
///
/// `(copia)` is NOT translated, and that is on purpose: it is a name written
/// to disk, not a label on the screen. A session in English would otherwise
/// make `foto (copy).png` next to the `foto (copia).png` an earlier session
/// made, and then the file the user is looking for is called whichever
/// language they happened to be in that day.
pub fn free_name(dir: &Path, name: &str, is_dir: bool) -> String {
    if !exists(&dir.join(name)) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && !is_dir => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1.. {
        let cand = if n == 1 {
            format!("{stem} (copia){ext}")
        } else {
            format!("{stem} (copia {n}){ext}")
        };
        if !exists(&dir.join(&cand)) {
            return cand;
        }
    }
    unreachable!()
}

/// Whether `inner` is `outer` or under it: a folder cannot go into itself.
pub fn is_within(inner: &Path, outer: &Path) -> bool {
    inner == outer || inner.starts_with(outer)
}

fn copy_tree(src: &Path, dst: &Path, prog: &Progress) -> io::Result<()> {
    if prog.cancel.load(Ordering::Relaxed) {
        return Err(cancelled());
    }
    let md = std::fs::symlink_metadata(src)?;
    let ft = md.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
    } else if ft.is_dir() {
        std::fs::create_dir(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()), prog)?;
        }
        std::fs::set_permissions(dst, md.permissions())
    } else {
        copy_file(src, dst, prog)
    }
}

/// Copy `src` (a file, a link or a whole folder) into `dir`, under a free
/// name. Returns where it went.
pub fn copy_into(src: &Path, dir: &Path) -> io::Result<PathBuf> {
    copy_into_with(src, dir, &Progress::default())
}

/// [`copy_into`], counting into `prog` (its total set here) and stoppable
/// through it. A cancelled copy leaves nothing behind.
pub fn copy_into_with(src: &Path, dir: &Path, prog: &Progress) -> io::Result<PathBuf> {
    let name = src.file_name().ok_or_else(bad_name)?.to_string_lossy().into_owned();
    if is_within(dir, src) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, T_SELF_NEST));
    }
    let is_dir = std::fs::symlink_metadata(src)?.is_dir();
    let dst = dir.join(free_name(dir, &name, is_dir));
    prog.total.store(tree_size(src), Ordering::Relaxed);
    match copy_tree(src, &dst, prog) {
        Ok(()) => Ok(dst),
        Err(e) => {
            // Half a copy is worse than none.
            let _ = std::fs::remove_dir_all(&dst).or_else(|_| std::fs::remove_file(&dst));
            Err(e)
        }
    }
}

/// Move `src` into `dir`, under a free name: a rename on the same disk, a
/// copy and then a removal across disks. Moving something to where it
/// already is does nothing.
pub fn move_into(src: &Path, dir: &Path) -> io::Result<PathBuf> {
    move_into_with(src, dir, &Progress::default())
}

/// [`move_into`], counting into `prog`. Only a move across disks copies, so
/// only that one has anything to count; on the same disk it is one rename.
pub fn move_into_with(src: &Path, dir: &Path, prog: &Progress) -> io::Result<PathBuf> {
    if src.parent() == Some(dir) {
        return Ok(src.to_path_buf());
    }
    let name = src.file_name().ok_or_else(bad_name)?.to_string_lossy().into_owned();
    if is_within(dir, src) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, T_SELF_NEST));
    }
    let is_dir = std::fs::symlink_metadata(src)?.is_dir();
    let dst = dir.join(free_name(dir, &name, is_dir));
    relocate(src, &dst, prog)?;
    Ok(dst)
}

/// `rename`, or copy-then-remove when `dst` is on another filesystem.
fn relocate(src: &Path, dst: &Path, prog: &Progress) -> io::Result<()> {
    match std::fs::rename(src, dst) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            prog.total.store(tree_size(src), Ordering::Relaxed);
            if let Err(e) = copy_tree(src, dst, prog) {
                // The original is still whole; drop the half copy.
                let _ = std::fs::remove_dir_all(dst).or_else(|_| std::fs::remove_file(dst));
                return Err(e);
            }
            if std::fs::symlink_metadata(src)?.is_dir() {
                std::fs::remove_dir_all(src)
            } else {
                std::fs::remove_file(src)
            }
        }
        r => r,
    }
}

/// Rename `path` to `name` in its own folder; refused when `name` is taken.
pub fn rename(path: &Path, name: &str) -> io::Result<PathBuf> {
    if !valid_name(name) {
        return Err(bad_name());
    }
    let dst = path.parent().ok_or_else(bad_name)?.join(name);
    if dst == path {
        return Ok(dst);
    }
    if exists(&dst) {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, T_EXISTS));
    }
    std::fs::rename(path, &dst)?;
    Ok(dst)
}

/// Make the folder `name` in `dir`.
pub fn make_dir(dir: &Path, name: &str) -> io::Result<PathBuf> {
    if !valid_name(name) {
        return Err(bad_name());
    }
    let p = dir.join(name);
    std::fs::create_dir(&p)?;
    Ok(p)
}

/// The trash: `$XDG_DATA_HOME/Trash`, or `~/.local/share/Trash`.
pub fn trash_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|d| d.join("Trash"))
}

/// `path` as the `.trashinfo` `Path=` key writes it: percent-encoded, with
/// `/` and the unreserved characters left as they are.
pub fn percent_path(path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `YYYY-MM-DDThh:mm:ss` in local time, as `DeletionDate=` wants it.
pub fn local_stamp(t: i64) -> String {
    let local = t + crate::files::local_offset(t);
    let (days, secs) = (local.div_euclid(86_400), local.rem_euclid(86_400));
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Send `path` to the trash in `trash` (see [`trash_dir`]): its `.trashinfo`
/// first, so a crash between the two leaves a stray info file and never a
/// trashed file nobody can trace back. Returns the name it has there.
pub fn trash_into(path: &Path, trash: &Path, now: i64) -> io::Result<String> {
    let abs = std::fs::canonicalize(path.parent().ok_or_else(bad_name)?)?
        .join(path.file_name().ok_or_else(bad_name)?);
    let (files, info) = (trash.join("files"), trash.join("info"));
    std::fs::create_dir_all(&files)?;
    std::fs::create_dir_all(&info)?;
    let name = abs.file_name().ok_or_else(bad_name)?.to_string_lossy().into_owned();
    let is_dir = std::fs::symlink_metadata(path)?.is_dir();
    // Taken in either half of the trash means taken.
    let mut free = free_name(&files, &name, is_dir);
    while exists(&info.join(format!("{free}.trashinfo"))) {
        free = free_name(&files, &format!("{free}~"), is_dir);
    }
    let info_path = info.join(format!("{free}.trashinfo"));
    let body = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_path(&abs.to_string_lossy()),
        local_stamp(now)
    );
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&info_path)?;
        f.write_all(body.as_bytes())?;
    }
    if let Err(e) = relocate(path, &files.join(&free), &Progress::default()) {
        let _ = std::fs::remove_file(&info_path);
        return Err(e);
    }
    Ok(free)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lunar-fileops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_taken_name_gets_a_copy_suffix_before_its_extension() {
        let d = tmp("names");
        assert_eq!(free_name(&d, "foto.png", false), "foto.png");
        std::fs::write(d.join("foto.png"), "").unwrap();
        assert_eq!(free_name(&d, "foto.png", false), "foto (copia).png");
        std::fs::write(d.join("foto (copia).png"), "").unwrap();
        assert_eq!(free_name(&d, "foto.png", false), "foto (copia 2).png");
        std::fs::create_dir(d.join("v1.2")).unwrap();
        assert_eq!(free_name(&d, "v1.2", true), "v1.2 (copia)", "a folder has no extension");
        std::fs::write(d.join(".bashrc"), "").unwrap();
        assert_eq!(free_name(&d, ".bashrc", false), ".bashrc (copia)");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn copying_and_moving_never_replace_and_a_folder_cannot_enter_itself() {
        let d = tmp("copy");
        let (a, b) = (d.join("a"), d.join("b"));
        std::fs::create_dir_all(a.join("sub")).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(a.join("sub/n.txt"), "hola").unwrap();
        std::fs::write(b.join("a"), "otro").unwrap();
        let c = copy_into(&a, &b).unwrap();
        assert_eq!(c, b.join("a (copia)"));
        assert_eq!(std::fs::read_to_string(c.join("sub/n.txt")).unwrap(), "hola");
        assert_eq!(std::fs::read_to_string(b.join("a")).unwrap(), "otro", "nothing replaced");
        assert!(copy_into(&a, &a.join("sub")).is_err());
        assert!(move_into(&a, &a.join("sub")).is_err());
        let m = move_into(&a.join("sub/n.txt"), &b).unwrap();
        assert_eq!(m, b.join("n.txt"));
        assert!(!a.join("sub/n.txt").exists());
        assert_eq!(move_into(&m, &b).unwrap(), m, "already there");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rename_and_new_folder_refuse_bad_and_taken_names() {
        let d = tmp("rename");
        std::fs::write(d.join("x"), "").unwrap();
        std::fs::write(d.join("y"), "").unwrap();
        for bad in ["", ".", "..", "a/b"] {
            assert!(rename(&d.join("x"), bad).is_err(), "{bad:?}");
            assert!(make_dir(&d, bad).is_err(), "{bad:?}");
        }
        assert!(rename(&d.join("x"), "y").is_err(), "taken");
        assert_eq!(rename(&d.join("x"), "z").unwrap(), d.join("z"));
        assert!(make_dir(&d, "nueva").unwrap().is_dir());
        assert!(make_dir(&d, "nueva").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_trash_keeps_what_it_takes_and_says_where_it_came_from() {
        let d = tmp("trash");
        let t = d.join("Trash");
        std::fs::write(d.join("mi nota.txt"), "x").unwrap();
        let n = trash_into(&d.join("mi nota.txt"), &t, 0).unwrap();
        assert_eq!(n, "mi nota.txt");
        assert!(!d.join("mi nota.txt").exists());
        assert!(t.join("files/mi nota.txt").exists());
        let info = std::fs::read_to_string(t.join("info/mi nota.txt.trashinfo")).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath=/"));
        assert!(info.contains("mi%20nota.txt\n"));
        assert!(info.contains("DeletionDate="));
        // A second file of the same name does not replace the first.
        std::fs::write(d.join("mi nota.txt"), "y").unwrap();
        assert_eq!(trash_into(&d.join("mi nota.txt"), &t, 0).unwrap(), "mi nota (copia).txt");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_copy_counts_its_bytes_and_a_cancelled_one_leaves_nothing() {
        let d = tmp("progress");
        let (src, out) = (d.join("big"), d.join("out"));
        std::fs::create_dir_all(src.join("in")).unwrap();
        std::fs::create_dir(&out).unwrap();
        std::fs::write(src.join("a"), vec![1u8; 3 << 20]).unwrap();
        std::fs::write(src.join("in/b"), vec![2u8; 1000]).unwrap();
        assert_eq!(tree_size(&src), (3 << 20) + 1000);
        let p = Progress::default();
        copy_into_with(&src, &out, &p).unwrap();
        assert_eq!(p.done.load(Ordering::Relaxed), (3 << 20) + 1000);
        assert_eq!(p.frac(), 1.0);
        let p = Progress::default();
        p.cancel.store(true, Ordering::Relaxed);
        let e = copy_into_with(&src, &out, &p).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Interrupted);
        assert!(!out.join("big (copia)").exists(), "a cancelled copy leaves nothing");
        assert_eq!(Progress::default().frac(), 1.0, "nothing to copy is done");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_deletion_date_is_a_calendar_date() {
        // The epoch in the local zone is still 1969 or 1970.
        let s = local_stamp(0);
        assert!(s.starts_with("1970-01-01T") || s.starts_with("1969-12-31T"), "{s}");
        assert_eq!(s.len(), 19);
        assert_eq!(percent_path("/a b/ñ"), "/a%20b/%C3%B1");
    }
}
