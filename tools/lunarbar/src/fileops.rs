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

/// A name a file can have: not empty, not `.` or `..`, no `/`, no NUL.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

fn bad_name() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "nombre no válido")
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// `name`, or the first of `stem (copia).ext`, `stem (copia 2).ext`, ... not
/// taken in `dir`. The extension stays last so the copy still opens with
/// what opened the original; a folder or a dotfile has none.
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

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let md = std::fs::symlink_metadata(src)?;
    let ft = md.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
    } else if ft.is_dir() {
        std::fs::create_dir(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        std::fs::set_permissions(dst, md.permissions())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// Copy `src` (a file, a link or a whole folder) into `dir`, under a free
/// name. Returns where it went.
pub fn copy_into(src: &Path, dir: &Path) -> io::Result<PathBuf> {
    let name = src.file_name().ok_or_else(bad_name)?.to_string_lossy().into_owned();
    if is_within(dir, src) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "una carpeta no cabe dentro de sí misma"));
    }
    let is_dir = std::fs::symlink_metadata(src)?.is_dir();
    let dst = dir.join(free_name(dir, &name, is_dir));
    match copy_tree(src, &dst) {
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
    if src.parent() == Some(dir) {
        return Ok(src.to_path_buf());
    }
    let name = src.file_name().ok_or_else(bad_name)?.to_string_lossy().into_owned();
    if is_within(dir, src) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "una carpeta no cabe dentro de sí misma"));
    }
    let is_dir = std::fs::symlink_metadata(src)?.is_dir();
    let dst = dir.join(free_name(dir, &name, is_dir));
    relocate(src, &dst)?;
    Ok(dst)
}

/// `rename`, or copy-then-remove when `dst` is on another filesystem.
fn relocate(src: &Path, dst: &Path) -> io::Result<()> {
    match std::fs::rename(src, dst) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_tree(src, dst)?;
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
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "ese nombre ya existe"));
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
    if let Err(e) = relocate(path, &files.join(&free)) {
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
    fn the_deletion_date_is_a_calendar_date() {
        // The epoch in the local zone is still 1969 or 1970.
        let s = local_stamp(0);
        assert!(s.starts_with("1970-01-01T") || s.starts_with("1969-12-31T"), "{s}");
        assert_eq!(s.len(), 19);
        assert_eq!(percent_path("/a b/ñ"), "/a%20b/%C3%B1");
    }
}
